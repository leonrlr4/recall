//! The MCP server against a real daemon router on an ephemeral port, driven
//! by an rmcp client over an in-process duplex pipe.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use chrono::{Local, TimeZone, Utc};
use recall_llm::Llm;
use recall_mcp::{PAGE_TOKENS, Recall, Url};
use recall_server::{
    AppState, ControlCommand, QaState, SelfTestResult, StatusSnapshot, build_router,
};
use recall_store::markdown::clock;
use recall_store::{
    ActionItem, Config, Index, Meeting, MeetingDir, MeetingId, Segment, Side, SpeakerId, Speakers,
    Step, StepState, Summary,
};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

const DAEMON_DOWN: &str =
    "recall daemon is not running; start it with: systemctl --user start recall";
const BUSY: &str = "meeting is being processed; try again when it is done";

struct FakeLlm;

#[async_trait]
impl Llm for FakeLlm {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
        Ok(String::new())
    }

    async fn stream(&self, _system: &str, _user: &str, _tx: mpsc::Sender<String>) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> String {
        "fake/test".into()
    }
}

struct Harness {
    _tmp: tempfile::TempDir,
    state: AppState,
    client: RunningService<RoleClient, ()>,
    weekly: MeetingId,
    long: MeetingId,
    alice: SpeakerId,
    controls: Arc<Mutex<Vec<ControlCommand>>>,
    reruns: Arc<Mutex<Vec<(MeetingId, Step)>>>,
}

fn meeting(
    id: &MeetingId,
    title: &str,
    segments: Vec<Segment>,
    summary: Option<Summary>,
) -> Meeting {
    let at = Utc.with_ymd_and_hms(2026, 10, 27, 1, 0, 0).unwrap();
    Meeting {
        id: id.clone(),
        title: title.into(),
        started: at,
        duration_ms: segments.last().map_or(0, |s| s.end_ms),
        source_app: Some("zoom".into()),
        steps: Step::ALL
            .iter()
            .map(|s| (*s, StepState::Done { at }))
            .collect(),
        segments,
        clusters: Vec::new(),
        summary,
        models: BTreeMap::new(),
        participants_hint: Vec::new(),
        screen_offset_ms: None,
        screen_error: None,
        sync: false,
    }
}

fn segment(
    start_ms: u32,
    end_ms: u32,
    side: Side,
    speaker: Option<&SpeakerId>,
    text: &str,
) -> Segment {
    Segment {
        start_ms,
        end_ms,
        side,
        speaker: speaker.cloned(),
        text: text.into(),
    }
}

/// Two hours, one segment every 5 s, with one segment longer than a whole
/// page in the middle.
fn long_segments(alice: &SpeakerId) -> Vec<Segment> {
    (0..1440u32)
        .map(|i| {
            let start = i * 5_000;
            let text = if i == 700 {
                "很長".repeat(PAGE_TOKENS + 10)
            } else {
                format!("Line {i}: we keep talking about the roadmap, 路線圖, and the budget.")
            };
            let (side, speaker) = if i % 3 == 0 {
                (Side::Me, None)
            } else if i % 3 == 1 {
                (Side::Them, Some(alice))
            } else {
                (Side::Them, None)
            };
            segment(start, start + 4_000, side, speaker, &text)
        })
        .collect()
}

async fn connect(base: Url) -> RunningService<RoleClient, ()> {
    connect_to(Recall::new(base).unwrap()).await
}

async fn connect_to(server: Recall) -> RunningService<RoleClient, ()> {
    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    ().serve(client_io).await.unwrap()
}

async fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Arc::new(Config {
        meetings_dir: tmp.path().join("Meetings"),
        data_dir: tmp.path().join("data"),
        summary_lang: "en".into(),
        port: addr.port(),
        ..Config::default()
    });
    let mut speakers = Speakers::default();
    let alice = speakers.add("Alice", Some(vec![1.0, 0.0]));
    speakers.save(&config.speakers_path()).unwrap();
    let index = Index::open(&config.index_path()).unwrap();

    let summary = Summary {
        one_line: "Release planning.".into(),
        body: "We agreed on the release date.".into(),
        decisions: vec!["Ship on Friday".into()],
        actions: vec![
            ActionItem {
                text: "Ship the release".into(),
                owner: Some("Alice".into()),
                due: Some("2026-11-01".into()),
            },
            ActionItem {
                text: "Write the notes".into(),
                owner: Some("Bob".into()),
                due: None,
            },
        ],
        chapters: Vec::new(),
    };
    let weekly_segments = vec![
        segment(0, 2_000, Side::Me, None, "Morning everyone."),
        segment(
            3_000,
            6_000,
            Side::Them,
            Some(&alice),
            "We ship the release on Friday.",
        ),
    ];
    let mut ids = Vec::new();
    for (hour, title, segments, summary) in [
        (9, "Weekly sync", weekly_segments, Some(summary)),
        (11, "Long planning", long_segments(&alice), None),
    ] {
        let started = Local.with_ymd_and_hms(2026, 10, 27, hour, 0, 0).unwrap();
        let dir = MeetingDir::create(&config.meetings_dir, started, title).unwrap();
        let m = meeting(&dir.id, title, segments, summary);
        m.save(&dir.path()).unwrap();
        index.upsert(&m, &speakers, false).unwrap();
        ids.push(dir.id);
    }

    let controls: Arc<Mutex<Vec<ControlCommand>>> = Arc::default();
    let reruns: Arc<Mutex<Vec<(MeetingId, Step)>>> = Arc::default();
    let (seen_controls, seen_reruns) = (controls.clone(), reruns.clone());
    let (events, _) = broadcast::channel(16);
    let state = AppState {
        config: config.clone(),
        config_path: tmp.path().join("config.toml"),
        index: Arc::new(Mutex::new(index)),
        speakers: Arc::new(Mutex::new(speakers)),
        events,
        llm: Arc::new(FakeLlm),
        status: Arc::new(RwLock::new(StatusSnapshot {
            phase: "idle".into(),
            since_ms: 42,
            source_app: None,
            mic_peak: 0.0,
            sys_peak: 0.0,
            meet: None,
            screen: None,
        })),
        control: Arc::new(move |c| {
            seen_controls.lock().unwrap().push(c);
            Ok(())
        }),
        selftest: Arc::new(|| {
            Ok(SelfTestResult {
                mic_peaks: Vec::new(),
                sys_peaks: Vec::new(),
            })
        }),
        rerun: Arc::new(move |id: &MeetingId, step: Step| {
            seen_reruns.lock().unwrap().push((id.clone(), step));
            Ok(7)
        }),
        meet: Arc::new(|batch| Ok(batch.events.len())),
        vault: None,
        qa: QaState::default(),
    };
    let router = build_router(state.clone());
    tokio::spawn(async move { axum_serve(listener, router).await });

    let client = connect(format!("http://{addr}").parse().unwrap()).await;
    let long = ids.pop().unwrap();
    let weekly = ids.pop().unwrap();
    Harness {
        _tmp: tmp,
        state,
        client,
        weekly,
        long,
        alice,
        controls,
        reruns,
    }
}

async fn axum_serve(listener: tokio::net::TcpListener, router: axum::Router) {
    axum::serve(listener, router).await.unwrap();
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    args: Value,
) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("arguments must be an object");
    };
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args))
        .await
        .unwrap()
}

fn text(result: &CallToolResult, block: usize) -> String {
    result.content[block]
        .as_text()
        .unwrap_or_else(|| panic!("block {block} is not text: {result:?}"))
        .text
        .clone()
}

fn is_error(result: &CallToolResult) -> bool {
    result.is_error == Some(true)
}

#[tokio::test]
async fn lists_all_tools_with_annotations() {
    let h = harness().await;
    let tools = h.client.list_all_tools().await.unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "get_meeting",
            "get_transcript",
            "list_action_items",
            "name_speaker",
            "recording_status",
            "rename_meeting",
            "rerun_step",
            "search_meetings",
            "start_recording",
            "stop_recording",
        ]
    );
    for name in &names {
        assert!(
            !name.contains("discard") && !name.contains("delete"),
            "{name} must not exist"
        );
    }
    for tool in &tools {
        let a = tool.annotations.as_ref().expect("annotated");
        let read_only = [
            "search_meetings",
            "get_meeting",
            "get_transcript",
            "list_action_items",
            "recording_status",
        ]
        .contains(&tool.name.as_ref());
        assert_eq!(a.read_only_hint, Some(read_only), "{}", tool.name);
        let idempotent = ["rename_meeting", "name_speaker"].contains(&tool.name.as_ref());
        if idempotent {
            assert_eq!(a.idempotent_hint, Some(true), "{}", tool.name);
        }
        let structured = ["list_action_items", "recording_status"].contains(&tool.name.as_ref());
        assert_eq!(tool.output_schema.is_some(), structured, "{}", tool.name);
    }
}

#[tokio::test]
async fn transcript_pages_cut_on_segment_boundaries() {
    let h = harness().await;
    let alice = h.alice.clone();
    let id = h.long.0.clone();

    let segments = long_segments(&alice);
    let full: String = segments
        .iter()
        .map(|s| {
            let name = match (&s.speaker, s.side) {
                (Some(_), _) => "Alice",
                (None, Side::Me) => "Me",
                (None, Side::Them) => "Them",
            };
            format!("[{}] {name}: {}\n", clock(s.start_ms), s.text)
        })
        .collect();

    let mut pages = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut args = json!({ "id": id });
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        let result = call(&h.client, "get_transcript", args).await;
        assert!(!is_error(&result), "{result:?}");
        let page = text(&result, 0);
        pages.push(page);
        match result.content.len() {
            1 => break,
            2 => {
                let next: Value = serde_json::from_str(&text(&result, 1)).unwrap();
                cursor = Some(next["next_cursor"].as_str().unwrap().to_string());
            }
            n => panic!("{n} content blocks"),
        }
        assert!(pages.len() < 100, "pagination does not end");
    }

    assert!(pages.len() > 3, "only {} pages", pages.len());
    assert_eq!(pages.concat(), full);
    for page in &pages {
        let lines = page.lines().count();
        let tokens = page.chars().count() / 2;
        assert!(
            tokens <= PAGE_TOKENS || lines == 1,
            "{lines} lines, {tokens} tokens"
        );
    }
    let alone = pages
        .iter()
        .find(|p| p.contains("很長很長"))
        .expect("the long segment is on some page");
    assert_eq!(alone.lines().count(), 1, "an oversized segment is alone");
    for pair in pages.windows(2) {
        let first_next = pair[1].lines().next().unwrap().chars().count() + 1;
        assert!(
            (pair[0].chars().count() + first_next) / 2 > PAGE_TOKENS,
            "a page ended early"
        );
    }
}

#[tokio::test]
async fn transcript_time_range() {
    let h = harness().await;
    let result = call(
        &h.client,
        "get_transcript",
        json!({ "id": h.long.0, "from_ms": 60_000, "to_ms": 70_000 }),
    )
    .await;
    assert_eq!(result.content.len(), 1, "{result:?}");
    let page = text(&result, 0);
    let starts: Vec<&str> = page.lines().map(|l| &l[..7]).collect();
    // Segments run [5i, 5i + 4) seconds; 56-60 s ends exactly at 60 s.
    assert_eq!(starts, ["[01:00]", "[01:05]"]);
}

#[tokio::test]
async fn action_items_structured() {
    let h = harness().await;
    let result = call(&h.client, "list_action_items", json!({ "owner": "alice" })).await;
    assert!(!is_error(&result), "{result:?}");
    assert_eq!(
        result.structured_content,
        Some(json!({
            "items": [{
                "meeting": h.weekly.0,
                "title": "Weekly sync",
                "started": "2026-10-27T01:00:00Z",
                "text": "Ship the release",
                "owner": "Alice",
                "due": "2026-11-01",
            }]
        }))
    );

    let result = call(&h.client, "list_action_items", json!({})).await;
    let items = &result.structured_content.unwrap()["items"];
    assert_eq!(items.as_array().unwrap().len(), 2);

    let result = call(
        &h.client,
        "list_action_items",
        json!({ "due_to": "2026-10-31" }),
    )
    .await;
    assert_eq!(result.structured_content, Some(json!({ "items": [] })));
}

#[tokio::test]
async fn every_tool_round_trips() {
    let h = harness().await;

    let status = call(&h.client, "recording_status", json!({})).await;
    let structured = status.structured_content.unwrap();
    assert_eq!(structured["phase"], "idle");
    assert_eq!(structured["since_ms"], 42);

    let found = text(
        &call(&h.client, "search_meetings", json!({ "query": "release" })).await,
        0,
    );
    assert!(found.contains(&h.weekly.0), "{found}");
    assert!(found.contains("[00:03] Alice:"), "{found}");

    let notes = text(
        &call(&h.client, "get_meeting", json!({ "id": h.weekly.0 })).await,
        0,
    );
    for want in [
        "# Weekly sync",
        "Ship on Friday",
        "Ship the release; owner: Alice; due: 2026-11-01",
        "Alice (speaker id",
    ] {
        assert!(notes.contains(want), "{want:?} missing from {notes}");
    }

    let missing = call(&h.client, "get_meeting", json!({ "id": "nope" })).await;
    assert!(is_error(&missing));
    assert!(text(&missing, 0).contains("404"), "{missing:?}");

    assert!(!is_error(
        &call(&h.client, "start_recording", json!({})).await
    ));
    assert!(!is_error(
        &call(&h.client, "stop_recording", json!({})).await
    ));
    assert_eq!(
        *h.controls.lock().unwrap(),
        [ControlCommand::Start, ControlCommand::Stop]
    );

    let rerun = call(
        &h.client,
        "rerun_step",
        json!({ "id": h.weekly.0, "step": "summarize" }),
    )
    .await;
    assert!(text(&rerun, 0).contains("job 7"), "{rerun:?}");
    assert_eq!(
        *h.reruns.lock().unwrap(),
        [(h.weekly.clone(), Step::Summarize)]
    );

    let renamed = call(
        &h.client,
        "rename_meeting",
        json!({ "id": h.weekly.0, "title": "Release sync" }),
    )
    .await;
    let reply = text(&renamed, 0);
    assert!(reply.contains("Release sync"), "{reply}");
    assert!(
        !reply.contains(&h.weekly.0),
        "the id follows the title: {reply}"
    );

    let named = call(
        &h.client,
        "name_speaker",
        json!({ "person_id": h.alice.0, "name": "Alice Wu" }),
    )
    .await;
    assert!(!is_error(&named), "{named:?}");
    let speakers = h.state.speakers.lock().unwrap();
    assert_eq!(speakers.name_of(&h.alice), "Alice Wu");
}

#[tokio::test]
async fn name_speaker_merges_into_a_person_with_that_name() {
    let h = harness().await;
    let (by_name, by_alias) = {
        let mut speakers = h.state.speakers.lock().unwrap();
        // Folding a person named Ally into Alice makes "Ally" her alias.
        let ally = speakers.add("Ally", None);
        speakers.merge(&h.alice, &ally).unwrap();
        (
            speakers.add_unnamed("Speaker 2", Some(vec![0.0, 1.0])),
            speakers.add_unnamed("Speaker 3", Some(vec![0.5, 0.5])),
        )
    };

    for (voice, name) in [(&by_name, " alice "), (&by_alias, "ALLY")] {
        let named = call(
            &h.client,
            "name_speaker",
            json!({ "person_id": voice.0, "name": name }),
        )
        .await;
        assert!(!is_error(&named), "{named:?}");
        assert!(text(&named, 0).contains(&h.alice.0), "{named:?}");
    }

    let speakers = h.state.speakers.lock().unwrap();
    assert!(speakers.get(&by_name).is_none());
    assert!(speakers.get(&by_alias).is_none());
    let alice = speakers.get(&h.alice).unwrap();
    assert_eq!(alice.name, "Alice");
    assert_eq!(alice.embeddings.len(), 3);
    assert_eq!(
        speakers
            .people()
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case("alice"))
            .count(),
        1
    );
}

#[tokio::test]
async fn daemon_down_error_message() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let client = connect(format!("http://{addr}").parse().unwrap()).await;
    for (name, args) in [
        ("recording_status", json!({})),
        ("search_meetings", json!({ "query": "x" })),
        ("rename_meeting", json!({ "id": "a", "title": "b" })),
    ] {
        let result = call(&client, name, args).await;
        assert!(is_error(&result), "{name}: {result:?}");
        assert_eq!(text(&result, 0), DAEMON_DOWN, "{name}");
    }
}

#[tokio::test]
async fn busy_meeting_maps_409() {
    let h = harness().await;
    h.state
        .index
        .lock()
        .unwrap()
        .enqueue(&h.weekly, Step::Summarize)
        .unwrap();
    for (name, args) in [
        (
            "rename_meeting",
            json!({ "id": h.weekly.0, "title": "Other" }),
        ),
        ("rerun_step", json!({ "id": h.weekly.0, "step": "write" })),
    ] {
        let result = call(&h.client, name, args).await;
        assert!(is_error(&result), "{name}: {result:?}");
        assert_eq!(text(&result, 0), BUSY, "{name}");
    }
}

fn set_sync(h: &Harness, id: &MeetingId, sync: bool) {
    let path = h.state.config.meetings_dir.join(&id.0);
    let mut meeting = Meeting::load(&path).unwrap();
    meeting.sync = sync;
    meeting.save(&path).unwrap();
}

/// What every read tool shows of `id` in synced-only mode: whether search
/// lists it or its transcript, and whether the per-meeting tools and the
/// action items reach it.
async fn visible(client: &RunningService<RoleClient, ()>, id: &MeetingId, word: &str) -> [bool; 5] {
    let search = text(
        &call(client, "search_meetings", json!({ "query": word })).await,
        0,
    );
    let in_rows = search.contains(&format!("- {}: ", id.0));
    let in_hits = search.contains(&format!("- {} [", id.0));
    let meeting = !is_error(&call(client, "get_meeting", json!({ "id": id.0 })).await);
    let transcript = call(client, "get_transcript", json!({ "id": id.0 })).await;
    if is_error(&transcript) {
        assert!(text(&transcript, 0).contains("404"), "{transcript:?}");
    }
    let items = call(client, "list_action_items", json!({})).await;
    let in_items = items.structured_content.unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["meeting"] == id.0);
    [
        in_rows || in_hits,
        in_hits,
        meeting,
        !is_error(&transcript),
        in_items,
    ]
}

#[tokio::test]
async fn synced_only_mode_hides_every_unsynced_meeting() {
    let h = harness().await;
    let base: Url = format!("http://127.0.0.1:{}", h.state.config.port)
        .parse()
        .unwrap();
    let client = connect_to(Recall::synced_only(base).unwrap()).await;

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.into_owned())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "get_meeting",
            "get_transcript",
            "list_action_items",
            "search_meetings"
        ]
    );
    for name in ["start_recording", "recording_status", "rename_meeting"] {
        let refused = client
            .call_tool(CallToolRequestParams::new(name))
            .await
            .map_or(true, |r| is_error(&r));
        assert!(refused, "{name} must not run in synced-only mode");
    }

    // Nothing synced: neither meeting shows through any tool.
    assert_eq!(visible(&client, &h.weekly, "release").await, [false; 5]);
    assert_eq!(visible(&client, &h.long, "roadmap").await, [false; 5]);
    let none = text(
        &call(&client, "search_meetings", json!({ "query": "release" })).await,
        0,
    );
    assert_eq!(none, "No meetings match.\n");

    // The weekly sync is published; the long planning stays private.
    set_sync(&h, &h.weekly, true);
    assert_eq!(visible(&client, &h.weekly, "release").await[1..], [true; 4]);
    assert_eq!(visible(&client, &h.long, "roadmap").await, [false; 5]);
    // The same meeting through the unrestricted server, for contrast.
    assert!(!is_error(
        &call(&h.client, "get_meeting", json!({ "id": h.long.0 })).await
    ));
}
