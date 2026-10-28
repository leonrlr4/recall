//! End-to-end tests: the router served on an ephemeral port, driven with
//! reqwest.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{Local, TimeZone, Utc};
use futures_util::StreamExt;
use recall_llm::Llm;
use recall_server::{AppState, EXTENSION_ORIGIN, SelfTestResult, StatusSnapshot, build_router};
use recall_store::{
    ActionItem, Config, Event, Index, MeetBatch, Meeting, MeetingDir, MeetingId, Segment, Side,
    SpeakerId, Speakers, Step, StepState, Summary, markdown,
};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

struct FakeLlm;

#[async_trait]
impl Llm for FakeLlm {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
        Ok("answer".into())
    }

    async fn stream(&self, _system: &str, user: &str, tx: mpsc::Sender<String>) -> Result<()> {
        assert!(user.contains("release"), "transcript missing from {user:?}");
        for token in ["The ", "answer", "."] {
            let _ = tx.send(token.to_string()).await;
        }
        Ok(())
    }

    fn name(&self) -> String {
        "fake/test".into()
    }
}

struct Harness {
    _tmp: tempfile::TempDir,
    base: String,
    config: Arc<Config>,
    state: AppState,
    reruns: Arc<Mutex<Vec<(MeetingId, Step)>>>,
    meet_batches: Arc<Mutex<Vec<MeetBatch>>>,
    alice: SpeakerId,
}

impl Harness {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn meeting_path(&self, id: &str) -> PathBuf {
        self.config.meetings_dir.join(id)
    }
}

fn encode(id: &str) -> String {
    id.replace(' ', "%20").replace(':', "%3A")
}

fn sample(id: &MeetingId, title: &str, alice: &SpeakerId, text: &str) -> Meeting {
    let at = Utc.with_ymd_and_hms(2026, 10, 27, 6, 30, 0).unwrap();
    let steps = Step::ALL
        .iter()
        .map(|s| (*s, StepState::Done { at }))
        .collect();
    Meeting {
        id: id.clone(),
        title: title.into(),
        started: at,
        duration_ms: 60_000,
        source_app: Some("zoom".into()),
        steps,
        segments: vec![
            Segment {
                start_ms: 0,
                end_ms: 2_000,
                side: Side::Me,
                speaker: None,
                text: "Morning everyone.".into(),
            },
            Segment {
                start_ms: 3_000,
                end_ms: 6_000,
                side: Side::Them,
                speaker: Some(alice.clone()),
                text: text.into(),
            },
        ],
        clusters: Vec::new(),
        summary: None,
        models: BTreeMap::new(),
        participants_hint: Vec::new(),
        screen_offset_ms: None,
        screen_error: None,
    }
}

async fn harness() -> Harness {
    harness_with(|_| {}).await
}

/// Like [`harness`], with `tweak` applied to the config.
async fn harness_with(tweak: impl FnOnce(&mut Config)) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    // Bound first: the server only accepts requests addressed to its
    // configured port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let mut config = Config {
        meetings_dir: tmp.path().join("Meetings"),
        data_dir: tmp.path().join("data"),
        summary_lang: "en".into(),
        port: addr.port(),
        ..Config::default()
    };
    tweak(&mut config);
    let config = Arc::new(config);
    let mut speakers = Speakers::default();
    let alice = speakers.add("Alice", Some(vec![1.0, 0.0]));
    speakers.save(&config.speakers_path()).unwrap();
    let index = Index::open(&config.index_path()).unwrap();

    let meetings = [
        (9, "Weekly sync", "We ship the release on Friday."),
        (11, "Design review", "The new layout looks good."),
    ];
    for (hour, title, text) in meetings {
        let started = Local.with_ymd_and_hms(2026, 10, 27, hour, 0, 0).unwrap();
        let dir = MeetingDir::create(&config.meetings_dir, started, title).unwrap();
        let meeting = sample(&dir.id, title, &alice, text);
        meeting.save(&dir.path()).unwrap();
        fs::write(dir.audio(), b"0123456789abcdef").unwrap();
        fs::write(
            dir.markdown(),
            markdown::render(&meeting, &speakers, &config.summary_lang),
        )
        .unwrap();
        index.upsert(&meeting, &speakers, true).unwrap();
    }

    let reruns: Arc<Mutex<Vec<(MeetingId, Step)>>> = Arc::default();
    let recorded = reruns.clone();
    let meet_batches: Arc<Mutex<Vec<MeetBatch>>> = Arc::default();
    let received = meet_batches.clone();
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
            since_ms: 0,
            source_app: None,
            mic_peak: 0.0,
            sys_peak: 0.0,
            meet: None,
            screen: None,
        })),
        control: Arc::new(|_| Ok(())),
        selftest: Arc::new(|| {
            Ok(SelfTestResult {
                mic_peaks: vec![0.5; 50],
                sys_peaks: vec![0.25; 50],
            })
        }),
        rerun: Arc::new(move |id: &MeetingId, step: Step| {
            recorded.lock().unwrap().push((id.clone(), step));
            Ok(1)
        }),
        meet: Arc::new(move |batch: MeetBatch| {
            let n = batch.events.len();
            received.lock().unwrap().push(batch);
            Ok(n)
        }),
    };

    let router = build_router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    Harness {
        _tmp: tmp,
        base: format!("http://{addr}"),
        config,
        state,
        reruns,
        meet_batches,
        alice,
    }
}

const WEEKLY: &str = "2026-10-27 09:00 Weekly sync";
const REVIEW: &str = "2026-10-27 11:00 Design review";

#[tokio::test]
async fn list_and_search_return_rows_and_hits() {
    let h = harness().await;
    let all: Value = reqwest::get(h.url("/api/meetings"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = all["meetings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [REVIEW, WEEKLY]);
    assert_eq!(all["meetings"][0]["participants"], json!(["Alice"]));
    assert_eq!(all["meetings"][0]["state"], "done");

    let found: Value = reqwest::get(h.url("/api/meetings?q=release"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["meetings"].as_array().unwrap().len(), 1);
    assert_eq!(found["meetings"][0]["id"], WEEKLY);
    let hits = found["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["meeting"], WEEKLY);
    assert_eq!(hits[0]["start_ms"], 3000);
    assert_eq!(hits[0]["speaker"], "Alice");
    assert!(hits[0]["snippet"].as_str().unwrap().contains("[release]"));

    let by_app: Value = reqwest::get(h.url("/api/meetings?app=slack"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(by_app["meetings"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn get_meeting_returns_names_and_markdown() {
    let h = harness().await;
    let res = reqwest::get(h.url(&format!("/api/meetings/{}", encode(WEEKLY))))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["meeting"]["title"], "Weekly sync");
    assert_eq!(body["names"][&h.alice.0], "Alice");
    assert_eq!(body["names"]["me"], "Me");
    assert!(body["markdown"].as_str().unwrap().contains("**Alice**"));

    let missing = reqwest::get(h.url("/api/meetings/2026-10-27%2009%3A00%20Nope"))
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn patch_title_renames_dir_and_md() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let res = client
        .patch(h.url(&format!("/api/meetings/{}", encode(WEEKLY))))
        .json(&json!({ "title": "Release planning" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let meeting: Value = res.json().await.unwrap();
    let new_id = "2026-10-27 09:00 Release planning";
    assert_eq!(meeting["id"], new_id);
    assert_eq!(meeting["title"], "Release planning");

    assert!(!h.meeting_path(WEEKLY).exists());
    let dir = h.meeting_path(new_id);
    let saved = Meeting::load(&dir).unwrap();
    assert_eq!(saved.id.0, new_id);
    let md = fs::read_to_string(dir.join("meeting.md")).unwrap();
    assert!(md.contains("title: Release planning"), "{md}");

    let rows: Value = reqwest::get(h.url("/api/meetings"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = rows["meetings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [REVIEW, new_id]);
}

#[tokio::test]
async fn patch_segment_text_rewrites_md() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let res = client
        .patch(h.url(&format!("/api/meetings/{}", encode(WEEKLY))))
        .json(&json!({ "segments": [
            { "index": 1, "text": "We ship the launch on Monday." },
            { "index": 0, "delete": true },
        ] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let meeting: Value = res.json().await.unwrap();
    assert_eq!(meeting["segments"].as_array().unwrap().len(), 1);
    assert_eq!(
        meeting["segments"][0]["text"],
        "We ship the launch on Monday."
    );

    let md = fs::read_to_string(h.meeting_path(WEEKLY).join("meeting.md")).unwrap();
    assert!(
        md.contains("**Alice** We ship the launch on Monday."),
        "{md}"
    );
    assert!(!md.contains("Morning everyone."), "{md}");

    let found: Value = reqwest::get(h.url("/api/meetings?q=launch"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(found["hits"].as_array().unwrap().len(), 1);

    let bad = client
        .patch(h.url(&format!("/api/meetings/{}", encode(WEEKLY))))
        .json(&json!({ "segments": [{ "index": 9, "text": "x" }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn delete_removes_dir_and_row() {
    let h = harness().await;
    let res = reqwest::Client::new()
        .delete(h.url(&format!("/api/meetings/{}", encode(REVIEW))))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert!(!h.meeting_path(REVIEW).exists());
    let rows: Value = reqwest::get(h.url("/api/meetings"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows["meetings"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn people_rename_applies_to_meeting_markdown() {
    let h = harness().await;
    let people: Value = reqwest::get(h.url("/api/people"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let alice = people
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == h.alice.0.as_str())
        .unwrap();
    assert_eq!(alice["name"], "Alice");
    assert_eq!(alice["samples"], 1);
    assert_eq!(alice["meetings"], 2);

    let res = reqwest::Client::new()
        .patch(h.url(&format!("/api/people/{}", h.alice.0)))
        .json(&json!({ "name": "Alice Chen" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    for id in [WEEKLY, REVIEW] {
        let md = fs::read_to_string(h.meeting_path(id).join("meeting.md")).unwrap();
        assert!(md.contains("**Alice Chen**"), "{md}");
    }
    let saved = Speakers::load(&h.config.speakers_path()).unwrap();
    assert_eq!(saved.name_of(&h.alice), "Alice Chen");
    let rows: Value = reqwest::get(h.url("/api/meetings?participant=Alice%20Chen"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rows["meetings"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn people_lists_unnamed_voices_and_renaming_names_them() {
    let h = harness().await;
    let stranger = {
        let mut speakers = h.state.speakers.lock().unwrap();
        let id = speakers.add_unnamed("Remote 1", Some(vec![0.0, 1.0]));
        speakers.save(&h.config.speakers_path()).unwrap();
        id
    };
    let people: Value = reqwest::get(h.url("/api/people"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let named = |id: &SpeakerId| {
        people
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id.0.as_str())
            .unwrap()["named"]
            .clone()
    };
    assert_eq!(named(&h.alice), true);
    assert_eq!(named(&stranger), false);

    let renamed: Value = reqwest::Client::new()
        .patch(h.url(&format!("/api/people/{}", stranger.0)))
        .json(&json!({ "name": "Bob" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(renamed["named"], true);
    let saved = Speakers::load(&h.config.speakers_path()).unwrap();
    assert!(saved.get(&stranger).unwrap().named);
}

#[tokio::test]
async fn people_merge_moves_segments_to_kept_person() {
    let h = harness().await;
    let bob = {
        let mut speakers = h.state.speakers.lock().unwrap();
        speakers.add("Bob", None)
    };
    let res = reqwest::Client::new()
        .patch(h.url(&format!("/api/people/{}", h.alice.0)))
        .json(&json!({ "merge_into": bob.0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let saved = Meeting::load(&h.meeting_path(WEEKLY)).unwrap();
    assert_eq!(saved.segments[1].speaker.as_ref(), Some(&bob));
    let md = fs::read_to_string(h.meeting_path(WEEKLY).join("meeting.md")).unwrap();
    assert!(md.contains("**Bob**"), "{md}");
    let speakers = Speakers::load(&h.config.speakers_path()).unwrap();
    assert!(speakers.get(&h.alice).is_none());
    assert_eq!(speakers.get(&bob).unwrap().aliases, ["Alice"]);
}

#[tokio::test]
async fn rerun_enqueues_job() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let res = client
        .post(h.url(&format!("/api/meetings/{}/rerun/summarize", encode(WEEKLY))))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body, json!({ "job": 1 }));
    assert_eq!(
        *h.reruns.lock().unwrap(),
        [(MeetingId(WEEKLY.into()), Step::Summarize)]
    );

    let bad = client
        .post(h.url(&format!("/api/meetings/{}/rerun/bogus", encode(WEEKLY))))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn audio_supports_range() {
    let h = harness().await;
    let res = reqwest::Client::new()
        .get(h.url(&format!("/api/meetings/{}/audio", encode(WEEKLY))))
        .header("Range", "bytes=2-5")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 206);
    assert_eq!(res.headers()["content-range"], "bytes 2-5/16");
    assert_eq!(res.headers()["content-type"], "audio/ogg");
    assert_eq!(res.bytes().await.unwrap().as_ref(), b"2345");
}

#[tokio::test]
async fn ask_streams_tokens_then_done() {
    let h = harness().await;
    let res = reqwest::Client::new()
        .post(h.url(&format!("/api/meetings/{}/ask", encode(WEEKLY))))
        .json(&json!({ "question": "When is the release?" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        res.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let body = res.text().await.unwrap();
    assert_eq!(
        body,
        "data: The \n\ndata: answer\n\ndata: .\n\nevent: done\ndata: end\n\n"
    );
}

#[tokio::test]
async fn ask_over_the_context_budget_errors_without_calling_the_model() {
    // 100 tokens leave a 60-character budget; the transcript alone is longer.
    let h = harness_with(|c| c.llm.context_tokens = 100).await;
    let body = reqwest::Client::new()
        .post(h.url(&format!("/api/meetings/{}/ask", encode(WEEKLY))))
        .json(&json!({ "question": "When is the release?" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        body,
        "event: error\ndata: this meeting is too long for the current context; \
         raise llm.context_tokens\n\nevent: done\ndata: end\n\n"
    );
}

#[tokio::test]
async fn events_stream_delivers_progress() {
    let h = harness().await;
    let res = reqwest::get(h.url("/api/events")).await.unwrap();
    assert_eq!(res.status(), 200);
    h.state
        .events
        .send(Event::Progress {
            meeting: MeetingId(WEEKLY.into()),
            step: Step::Transcribe,
            pct: 0.5,
        })
        .unwrap();
    let mut stream = res.bytes_stream();
    let mut seen = String::new();
    while !seen.contains("\n\n") {
        let chunk = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("no event within 5 s")
            .unwrap()
            .unwrap();
        seen.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    let data = seen.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
    let event: Value = serde_json::from_str(data).unwrap();
    assert_eq!(
        event,
        json!({ "kind": "progress", "meeting": WEEKLY, "step": "transcribe", "pct": 0.5 })
    );
}

#[tokio::test]
async fn status_record_and_selftest() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let status: Value = reqwest::get(h.url("/api/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["phase"], "idle");
    let start = client
        .post(h.url("/api/record/start"))
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 204);
    let screen_off = client
        .post(h.url("/api/record/screen_off"))
        .send()
        .await
        .unwrap();
    assert_eq!(screen_off.status(), 204);
    let bad = client
        .post(h.url("/api/record/pause"))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    let selftest: Value = client
        .post(h.url("/api/selftest"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(selftest["mic_peaks"].as_array().unwrap().len(), 50);
}

#[tokio::test]
async fn spa_fallback_serves_index_html() {
    let h = harness().await;
    for path in ["/", "/meetings/anything", "/index.html"] {
        let res = reqwest::get(h.url(path)).await.unwrap();
        assert_eq!(res.status(), 200, "{path}");
        assert!(
            res.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html"),
            "{path}"
        );
        assert!(res.text().await.unwrap().contains("<title>recall</title>"));
    }
    let api = reqwest::get(h.url("/api/nope")).await.unwrap();
    assert_eq!(api.status(), 404);
}

#[tokio::test]
async fn config_get_and_put() {
    let h = harness().await;
    let config: Value = reqwest::get(h.url("/api/config"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(config["summary_lang"], "en");

    let path = h.state.config_path.clone();
    fs::write(
        &path,
        "summary_lang = \"en\"\n[llm]\nbackend = \"claude\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let client = reqwest::Client::new();
    let put = |body: Value| client.put(h.url("/api/config")).json(&body).send();

    let res = put(json!({ "port": 9000, "llm": { "model": "n" } }))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.json::<Value>().await.unwrap(),
        json!({ "restart_required": true })
    );
    let saved = Config::load_from(&path).unwrap();
    assert_eq!(saved.port, 9000);
    assert_eq!(saved.summary_lang, "en", "unsent key was reset");
    assert_eq!(saved.llm.backend, "claude", "unsent section key was reset");
    assert_eq!(saved.llm.model, "n");

    for (body, needle) in [
        (json!({ "prot": 1 }), "prot"),
        (json!({ "llm": { "modle": "x" } }), "llm.modle"),
        (json!({ "port": 0 }), "port"),
        (json!({ "data_dir": "rel/dir" }), "data_dir"),
        (json!({ "port": "x" }), "invalid config"),
        (json!({ "stt": { "engine": "whisper" } }), "stt.engine"),
        (json!([1]), "object"),
    ] {
        let res = put(body.clone()).await.unwrap();
        assert_eq!(res.status(), 400, "{body}");
        let error: Value = res.json().await.unwrap();
        assert!(
            error["error"].as_str().unwrap().contains(needle),
            "{body}: {error}"
        );
    }
    assert_eq!(Config::load_from(&path).unwrap().port, 9000);
    // Written through a temporary file that is renamed into place.
    let names: Vec<_> = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != "Meetings" && n != "data")
        .collect();
    assert_eq!(names, ["config.toml"]);
}

#[tokio::test]
async fn cross_origin_requests_are_forbidden() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let evil_origin = client
        .post(h.url("/api/record/discard"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(evil_origin.status(), 403);
    let evil_host = client
        .get(h.url("/api/meetings"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(evil_host.status(), 403);
    let evil_spa = client
        .get(h.url("/"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(evil_spa.status(), 403);

    let port = h.config.port;
    let same_origin = client
        .post(h.url("/api/record/stop"))
        .header("Origin", format!("http://localhost:{port}"))
        .header("Host", format!("localhost:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(same_origin.status(), 204);
    let plain = client.get(h.url("/api/status")).send().await.unwrap();
    assert_eq!(plain.status(), 200);
}

#[tokio::test]
async fn people_patch_rejects_name_with_merge() {
    let h = harness().await;
    let res = reqwest::Client::new()
        .patch(h.url(&format!("/api/people/{}", h.alice.0)))
        .json(&json!({ "name": "A", "merge_into": "me" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let saved = Speakers::load(&h.config.speakers_path()).unwrap();
    assert_eq!(saved.name_of(&h.alice), "Alice");
}

#[tokio::test]
async fn busy_meeting_refuses_edit_delete_and_rerun() {
    let h = harness().await;
    h.state
        .index
        .lock()
        .unwrap()
        .enqueue(&MeetingId(WEEKLY.into()), Step::Segment)
        .unwrap();
    let client = reqwest::Client::new();
    let url = h.url(&format!("/api/meetings/{}", encode(WEEKLY)));
    let responses = [
        client
            .patch(&url)
            .json(&json!({ "title": "Renamed" }))
            .send()
            .await
            .unwrap(),
        client.delete(&url).send().await.unwrap(),
        client
            .post(format!("{url}/rerun/summarize"))
            .send()
            .await
            .unwrap(),
    ];
    for res in responses {
        assert_eq!(res.status(), 409);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body, json!({ "error": "meeting is being processed" }));
    }
    assert!(h.meeting_path(WEEKLY).join("meeting.json").is_file());
    assert!(h.reruns.lock().unwrap().is_empty());

    // Another meeting is unaffected.
    let res = client
        .delete(h.url(&format!("/api/meetings/{}", encode(REVIEW))))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
}

#[tokio::test]
async fn selftest_refuses_while_recording() {
    let h = harness().await;
    h.state.status.write().unwrap().phase = "recording".into();
    let res = reqwest::Client::new()
        .post(h.url("/api/selftest"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 409);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body, json!({ "error": "recording in progress" }));
}

#[tokio::test]
async fn people_patch_refuses_while_a_job_runs() {
    let h = harness().await;
    {
        let index = h.state.index.lock().unwrap();
        index
            .enqueue(&MeetingId(WEEKLY.into()), Step::Segment)
            .unwrap();
        index.next_job().unwrap().unwrap();
    }
    let res = reqwest::Client::new()
        .patch(h.url(&format!("/api/people/{}", h.alice.0)))
        .json(&json!({ "name": "Alice Chen" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 409);
    let saved = Speakers::load(&h.config.speakers_path()).unwrap();
    assert_eq!(saved.name_of(&h.alice), "Alice");
}

const MEET_BODY: &str = r#"{"meeting_code": "abc-defg-hij", "events": [
    {"t": 1793170000123, "kind": "speaking_start", "participant": "csrc:12345"}]}"#;

#[tokio::test]
async fn extension_origin_allowed_only_on_meet_events() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let post = |path: &str, origin: &str| {
        client
            .post(h.url(path))
            .header("Origin", origin)
            .header("Content-Type", "application/json")
            .body(MEET_BODY)
            .send()
    };

    let res = post("/api/meet/events", EXTENSION_ORIGIN).await.unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(
        res.headers()["access-control-allow-origin"],
        EXTENSION_ORIGIN
    );
    {
        let batches = h.meet_batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].meeting_code, "abc-defg-hij");
    }

    let res = post("/api/record/start", EXTENSION_ORIGIN).await.unwrap();
    assert_eq!(res.status(), 403);
    let res = post("/api/meet/events", "chrome-extension://otherid")
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    let res = post("/api/meet/events", "https://meet.google.com")
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    // The Host must still be loopback.
    let res = client
        .post(h.url("/api/meet/events"))
        .header("Origin", EXTENSION_ORIGIN)
        .header("Host", "evil.example")
        .body(MEET_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    // Without an Origin it is an ordinary same-origin request.
    let res = client
        .post(h.url("/api/meet/events"))
        .body(MEET_BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert!(res.headers().get("access-control-allow-origin").is_none());
    assert_eq!(h.meet_batches.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn extension_preflight_is_answered() {
    let h = harness().await;
    let client = reqwest::Client::new();
    let preflight = |path: &str, origin: &str| {
        client
            .request(reqwest::Method::OPTIONS, h.url(path))
            .header("Origin", origin)
            .header("Access-Control-Request-Method", "POST")
            .header("Access-Control-Request-Headers", "content-type")
            .send()
    };

    let res = preflight("/api/meet/events", EXTENSION_ORIGIN)
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let headers = res.headers();
    assert_eq!(headers["access-control-allow-origin"], EXTENSION_ORIGIN);
    assert_eq!(headers["access-control-allow-methods"], "POST");
    assert_eq!(headers["access-control-allow-headers"], "Content-Type");
    assert!(h.meet_batches.lock().unwrap().is_empty());

    let res = preflight("/api/record/start", EXTENSION_ORIGIN)
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    let res = preflight("/api/meet/events", "chrome-extension://otherid")
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn meet_events_rejects_bad_body() {
    let h = harness().await;
    let client = reqwest::Client::new();
    for body in [
        "not json",
        "{}",
        r#"{"meeting_code": "abc", "events": [{"kind": "status", "status": "ok"}]}"#,
        r#"{"meeting_code": "abc", "events": [{"t": "soon", "kind": "status"}]}"#,
        "[]",
        "",
    ] {
        let res = client
            .post(h.url("/api/meet/events"))
            .header("Origin", EXTENSION_ORIGIN)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400, "{body:?}");
    }
    assert!(h.meet_batches.lock().unwrap().is_empty());
}

fn set_actions(h: &Harness, id: &str, actions: Vec<ActionItem>) {
    let path = h.meeting_path(id);
    let mut meeting = Meeting::load(&path).unwrap();
    meeting.summary = Some(Summary {
        one_line: String::new(),
        body: String::new(),
        decisions: Vec::new(),
        actions,
        chapters: Vec::new(),
    });
    meeting.save(&path).unwrap();
}

fn action(text: &str, owner: Option<&str>, due: Option<&str>) -> ActionItem {
    ActionItem {
        text: text.into(),
        owner: owner.map(Into::into),
        due: due.map(Into::into),
    }
}

#[tokio::test]
async fn action_items_filters_by_owner_and_due() {
    let h = harness().await;
    set_actions(
        &h,
        WEEKLY,
        vec![
            action("Write release notes", Some("Leon"), Some("2026-10-30")),
            action("Book the room", None, None),
        ],
    );
    set_actions(
        &h,
        REVIEW,
        vec![
            action("Update the mockups", Some("Amy Chen"), Some("2026-11-05")),
            action("Check contrast", Some("leon"), None),
        ],
    );
    let get = |query: &str| {
        let url = h.url(&format!("/api/action-items{query}"));
        async move {
            let res = reqwest::get(url).await.unwrap();
            assert_eq!(res.status(), 200);
            let items: Vec<Value> = res.json().await.unwrap();
            items
                .iter()
                .map(|i| i["text"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    // Newest meeting first, items in summary order.
    assert_eq!(
        get("").await,
        [
            "Update the mockups",
            "Check contrast",
            "Write release notes",
            "Book the room"
        ]
    );
    let all: Vec<Value> = reqwest::get(h.url("/api/action-items"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all[0]["meeting"], REVIEW);
    assert_eq!(all[0]["title"], "Design review");
    assert_eq!(all[0]["owner"], "Amy Chen");
    assert_eq!(all[0]["due"], "2026-11-05");
    assert!(
        all[0]["started"]
            .as_str()
            .unwrap()
            .starts_with("2026-10-27")
    );
    assert_eq!(all[3]["owner"], Value::Null);
    assert_eq!(all[3]["due"], Value::Null);

    assert_eq!(
        get("?owner=LEON").await,
        ["Check contrast", "Write release notes"]
    );
    assert_eq!(get("?owner=chen").await, ["Update the mockups"]);
    assert_eq!(get("?owner=").await.len(), 4);
    // A due bound leaves out items without a due date.
    assert_eq!(get("?due_from=2026-10-31").await, ["Update the mockups"]);
    assert_eq!(get("?due_to=2026-10-30").await, ["Write release notes"]);
    assert_eq!(
        get("?owner=leon&due_from=2026-10-01&due_to=2026-12-31").await,
        ["Write release notes"]
    );
    // Both sample meetings started at 06:30 UTC.
    assert!(get("?to=2026-10-27T06:00:00Z").await.is_empty());
    assert_eq!(
        get("?from=2026-10-27T06:00:00Z&to=2026-10-27T07:00:00Z&owner=amy").await,
        ["Update the mockups"]
    );
}
