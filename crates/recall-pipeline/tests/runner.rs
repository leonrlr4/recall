//! The step runner with fake engines on a generated recording.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono::{Local, Utc};
use recall_llm::Llm;
use recall_pipeline::runner::{Engines, run_from};
use recall_pipeline::{Diarizer, Region, Transcriber, Turn, Utterance, Vad};
use recall_store::speakers::ME;
use recall_store::{
    Cluster, Config, Index, ListFilter, MeetEvent, MeetEventKind, Meeting, MeetingDir, NamedBy,
    Segment, Side, SpeakerId, Speakers, Step, StepState, append_events,
};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Calls made to each fake, shared with the test.
#[derive(Clone, Default)]
struct Calls {
    transcribe: Arc<AtomicUsize>,
    diarize: Arc<AtomicUsize>,
    llm: Arc<AtomicUsize>,
}

/// One region from the first to the last sample louder than 0.1, or none.
struct FakeVad;

impl Vad for FakeVad {
    fn regions(&self, samples16k: &[f32]) -> Result<Vec<Region>> {
        let loud = |s: &f32| s.abs() > 0.1;
        let (Some(first), Some(last)) = (
            samples16k.iter().position(loud),
            samples16k.iter().rposition(loud),
        ) else {
            return Ok(Vec::new());
        };
        let ms = |i: usize| u32::try_from(i / 16).unwrap();
        Ok(vec![Region {
            start_ms: ms(first),
            end_ms: ms(last + 1),
        }])
    }
}

/// One "hello" per region.
struct FakeTranscriber(Arc<AtomicUsize>);

impl Transcriber for FakeTranscriber {
    fn transcribe(&mut self, _: &[f32], regions: &[Region], lang: &str) -> Result<Vec<Utterance>> {
        assert_eq!(lang, "zh");
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(regions
            .iter()
            .map(|r| Utterance {
                start_ms: r.start_ms,
                end_ms: r.end_ms,
                text: "hello".into(),
                words: Vec::new(),
            })
            .collect())
    }

    fn model_name(&self) -> String {
        "fake-whisper".into()
    }
}

/// One cluster talking through the whole recording.
struct FakeDiarizer(Arc<AtomicUsize>);

impl Diarizer for FakeDiarizer {
    fn diarize(&mut self, samples16k: &[f32]) -> Result<(Vec<Turn>, Vec<Vec<f32>>)> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let end_ms = u32::try_from(samples16k.len() / 16).unwrap();
        Ok((
            vec![Turn {
                start_ms: 0,
                end_ms,
                cluster: 0,
            }],
            vec![vec![1.0, 0.0, 0.0]],
        ))
    }
}

const SUMMARY: &str = r#"{"one_line":"Said hello.","body":"Both sides said hello.","decisions":[],"actions":[],"chapters":[{"start_ms":0,"title":"Greetings"}]}"#;

/// A fixed summary, or an error when `fail` is set.
struct FakeLlm {
    calls: Arc<AtomicUsize>,
    fail: bool,
}

#[async_trait]
impl Llm for FakeLlm {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            bail!("model unreachable");
        }
        Ok(SUMMARY.into())
    }

    async fn stream(&self, system: &str, user: &str, tx: mpsc::Sender<String>) -> Result<()> {
        let _ = tx.send(self.complete(system, user).await?).await;
        Ok(())
    }

    fn name(&self) -> String {
        "fake/llm".into()
    }
}

fn engines(calls: &Calls, llm_fails: bool) -> Engines {
    Engines {
        vad: Box::new(FakeVad),
        transcriber: Box::new(FakeTranscriber(calls.transcribe.clone())),
        diarizer: Box::new(FakeDiarizer(calls.diarize.clone())),
        llm: Arc::new(FakeLlm {
            calls: calls.llm.clone(),
            fail: llm_fails,
        }),
    }
}

struct Fixture {
    _tmp: TempDir,
    config: Config,
    dir: MeetingDir,
}

/// A meeting with a 2 s stereo `audio.opus`: a tone on the microphone (left)
/// for the first second and on the system audio (right) for the second.
fn fixture() -> Fixture {
    fixture_with(
        "aevalsrc=if(lt(t\\,1)\\,0.5*sin(2*PI*440*t)\\,0)\
         |if(gte(t\\,1)\\,0.5*sin(2*PI*440*t)\\,0):s=48000:d=2",
    )
}

/// Like [`fixture`], with the audio from the ffmpeg `lavfi` source `source`.
fn fixture_with(source: &str) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let config = Config {
        meetings_dir: tmp.path().join("meetings"),
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    let dir = MeetingDir::create(&config.meetings_dir, Local::now(), "Runner test").unwrap();
    write_audio(&dir.audio(), source);
    let meeting = Meeting {
        id: dir.id.clone(),
        title: "Runner test".into(),
        started: Utc::now(),
        duration_ms: 2_000,
        source_app: Some("Google Meet".into()),
        steps: Step::ALL.iter().map(|s| (*s, StepState::Pending)).collect(),
        segments: Vec::new(),
        clusters: Vec::new(),
        summary: None,
        models: BTreeMap::new(),
        participants_hint: Vec::new(),
        screen_offset_ms: None,
        screen_error: None,
        sync: false,
    };
    meeting.save(&dir.path()).unwrap();
    Fixture {
        _tmp: tmp,
        config,
        dir,
    }
}

fn write_audio(path: &Path, source: &str) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(source)
        .args(["-c:a", "libopus", "-b:a", "64k"])
        .arg(path)
        .status()
        .expect("running ffmpeg");
    assert!(
        status.success(),
        "ffmpeg failed to write {}",
        path.display()
    );
}

fn done_at(meeting: &Meeting, step: Step) -> chrono::DateTime<Utc> {
    match meeting.steps.get(&step) {
        Some(StepState::Done { at }) => *at,
        other => panic!("{step:?} is {other:?}, not done"),
    }
}

#[tokio::test]
async fn run_from_segment_reaches_write_with_fakes() {
    let f = fixture();
    let calls = Calls::default();
    let mut engines = engines(&calls, false);
    let mut speakers = Speakers::default();
    let progress = Mutex::new(Vec::new());

    run_from(
        &mut engines,
        &f.config,
        &f.dir,
        &mut speakers,
        Step::Segment,
        |step, pct, done| progress.lock().unwrap().push((step, pct, done)),
    )
    .await
    .unwrap();

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    for step in Step::ALL {
        done_at(&meeting, step);
    }
    let progress = progress.into_inner().unwrap();
    assert_eq!(
        progress
            .iter()
            .filter(|(_, _, done)| *done)
            .map(|(s, _, _)| *s)
            .collect::<Vec<_>>(),
        Step::ALL
    );
    assert!((progress.last().unwrap().1 - 1.0).abs() < 1e-6);
    // Transcribe reports each side's piece before the step is done, within
    // its own sixth of the run.
    let pieces: Vec<f32> = progress
        .iter()
        .filter(|(s, _, done)| *s == Step::Transcribe && !*done)
        .map(|(_, pct, _)| *pct)
        .collect();
    let n = Step::ALL.len() as f32;
    assert_eq!(pieces.len(), 2);
    assert!((pieces[0] - 1.5 / n).abs() < 1e-6, "{pieces:?}");
    assert!((pieces[1] - 2.0 / n).abs() < 1e-6, "{pieces:?}");

    // One "hello" per side; the mic region survives the echo rule because
    // the two tones barely overlap.
    let sides: Vec<Side> = meeting.segments.iter().map(|s| s.side).collect();
    assert_eq!(sides, [Side::Me, Side::Them]);
    assert!(meeting.segments.iter().all(|s| s.text == "hello"));
    assert_eq!(calls.diarize.load(Ordering::SeqCst), 1);
    assert_eq!(meeting.clusters.len(), 1);
    let remote = meeting.segments[1].speaker.clone().expect("remote speaker");
    assert_eq!(meeting.clusters[0].speaker.as_ref(), Some(&remote));
    assert_eq!(meeting.summary.as_ref().unwrap().one_line, "Said hello.");
    assert_eq!(meeting.models["transcriber"], "fake-whisper");
    assert_eq!(meeting.models["llm"], "fake/llm");
    assert_eq!(meeting.duration_ms, 2_000);

    // The new remote voice is registered and saved.
    let saved = Speakers::load(&f.config.speakers_path()).unwrap();
    assert!(saved.get(&remote).is_some());
    assert!(speakers.get(&remote).is_some());

    let md = std::fs::read_to_string(f.dir.markdown()).unwrap();
    assert!(md.contains("Said hello."), "{md}");
    let rows = Index::open(&f.config.index_path())
        .unwrap()
        .list(&ListFilter::default())
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, f.dir.id);
    assert_eq!(rows[0].state, "done");
}

#[tokio::test]
async fn run_from_summarize_only_reruns_last_steps() {
    let f = fixture();
    let calls = Calls::default();
    let mut engines = engines(&calls, false);
    let mut speakers = Speakers::default();
    run_from(
        &mut engines,
        &f.config,
        &f.dir,
        &mut speakers,
        Step::Segment,
        |_, _, _| {},
    )
    .await
    .unwrap();
    let first = Meeting::load(&f.dir.path()).unwrap();

    let progress = Mutex::new(Vec::new());
    run_from(
        &mut engines,
        &f.config,
        &f.dir,
        &mut speakers,
        Step::Summarize,
        |step, pct, done| progress.lock().unwrap().push((step, pct, done)),
    )
    .await
    .unwrap();
    let second = Meeting::load(&f.dir.path()).unwrap();

    assert_eq!(
        calls.transcribe.load(Ordering::SeqCst),
        2,
        "one per side, once"
    );
    assert_eq!(calls.diarize.load(Ordering::SeqCst), 1);
    assert_eq!(calls.llm.load(Ordering::SeqCst), 2);
    for step in [Step::Segment, Step::Transcribe, Step::Diarize, Step::Name] {
        assert_eq!(done_at(&first, step), done_at(&second, step), "{step:?}");
    }
    for step in [Step::Summarize, Step::Write] {
        assert!(done_at(&second, step) > done_at(&first, step), "{step:?}");
    }
    assert_eq!(second.segments, first.segments);
    assert_eq!(second.clusters, first.clusters);
    let progress = progress.into_inner().unwrap();
    assert_eq!(
        progress,
        [(Step::Summarize, 5.0 / 6.0, true), (Step::Write, 1.0, true)]
    );
}

#[tokio::test]
async fn summary_chunk_size_follows_context_tokens() {
    let mut f = fixture();
    // 50 tokens give a 30-character budget, less than the two lines, so
    // the summary takes one call per line and one merge.
    f.config.llm.context_tokens = 50;
    let calls = Calls::default();
    let mut engines = engines(&calls, false);
    run_from(
        &mut engines,
        &f.config,
        &f.dir,
        &mut Speakers::default(),
        Step::Segment,
        |_, _, _| {},
    )
    .await
    .unwrap();
    assert_eq!(calls.llm.load(Ordering::SeqCst), 3);
}

/// Enables `claude_code` with a fake `claude` that records each call in
/// `called` beside itself and then runs `body`, and runs the whole pipeline
/// on a meeting whose `sync` flag is `sync`.
async fn run_with_claude_code(sync: bool, body: &str) -> (Fixture, Calls, Meeting) {
    use std::os::unix::fs::PermissionsExt;

    let mut f = fixture();
    let bin = f._tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let script = bin.join("claude");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat > /dev/null\ntouch called\n{body}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.config.claude_code.enabled = true;
    f.config.claude_code.command = script.to_string_lossy().into_owned();
    f.config.claude_code.cwd = bin;
    let mut meeting = Meeting::load(&f.dir.path()).unwrap();
    meeting.sync = sync;
    meeting.save(&f.dir.path()).unwrap();

    let calls = Calls::default();
    let mut engines = engines(&calls, false);
    run_from(
        &mut engines,
        &f.config,
        &f.dir,
        &mut Speakers::default(),
        Step::Segment,
        |_, _, _| {},
    )
    .await
    .unwrap();
    let meeting = Meeting::load(&f.dir.path()).unwrap();
    (f, calls, meeting)
}

const CLAUDE_CODE_REPLY: &str = r#"echo '{"type":"result","subtype":"success","is_error":false,"result":"{\"one_line\":\"TRL6 review.\",\"body\":\"b\"}","modelUsage":{"claude-opus-5-5":{"outputTokens":9}}}'"#;

#[tokio::test]
async fn synced_meeting_is_summarized_by_claude_code() {
    let (f, calls, meeting) = run_with_claude_code(true, CLAUDE_CODE_REPLY).await;

    assert!(f._tmp.path().join("bin/called").exists());
    assert_eq!(calls.llm.load(Ordering::SeqCst), 0);
    assert_eq!(meeting.summary.unwrap().one_line, "TRL6 review.");
    assert_eq!(meeting.models["llm"], "claude-code/claude-opus-5-5");
}

#[tokio::test]
async fn meeting_not_synced_keeps_the_local_backend() {
    let (f, calls, meeting) = run_with_claude_code(false, CLAUDE_CODE_REPLY).await;

    assert!(!f._tmp.path().join("bin/called").exists());
    assert_eq!(calls.llm.load(Ordering::SeqCst), 1);
    assert_eq!(meeting.models["llm"], "fake/llm");
}

#[tokio::test]
async fn failed_claude_code_run_falls_back_to_the_local_backend() {
    let (f, calls, meeting) = run_with_claude_code(true, "echo 'Not logged in' >&2\nexit 1").await;

    assert!(f._tmp.path().join("bin/called").exists());
    assert_eq!(calls.llm.load(Ordering::SeqCst), 1);
    assert_eq!(meeting.summary.as_ref().unwrap().one_line, "Said hello.");
    assert_eq!(meeting.models["llm"], "fake/llm");
    done_at(&meeting, Step::Summarize);
}

#[tokio::test]
async fn failed_step_is_recorded_and_later_steps_stay_pending() {
    let f = fixture();
    let calls = Calls::default();
    let mut engines = engines(&calls, true);
    let mut speakers = Speakers::default();

    let err = run_from(
        &mut engines,
        &f.config,
        &f.dir,
        &mut speakers,
        Step::Segment,
        |_, _, _| {},
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("model unreachable"), "{err:#}");

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    for step in [Step::Segment, Step::Transcribe, Step::Diarize, Step::Name] {
        done_at(&meeting, step);
    }
    match &meeting.steps[&Step::Summarize] {
        StepState::Failed { error, .. } => assert!(error.contains("model unreachable"), "{error}"),
        other => panic!("summarize is {other:?}"),
    }
    assert_eq!(meeting.steps[&Step::Write], StepState::Pending);
    assert!(!f.dir.markdown().exists());
}

#[tokio::test]
async fn mic_only_meeting_gives_every_line_to_me() {
    // A tone on the microphone only; the system audio is silent.
    let f = fixture_with("aevalsrc=0.5*sin(2*PI*440*t)|0:s=48000:d=2");
    let calls = Calls::default();
    let mut engines = engines(&calls, false);
    let mut speakers = Speakers::default();

    run_from(
        &mut engines,
        &f.config,
        &f.dir,
        &mut speakers,
        Step::Segment,
        |_, _, _| {},
    )
    .await
    .unwrap();

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    assert!(!meeting.segments.is_empty());
    for segment in &meeting.segments {
        assert_eq!(segment.side, Side::Me);
        assert_eq!(segment.speaker, Some(SpeakerId(ME.into())));
    }
    assert!(meeting.clusters.is_empty());
    assert_eq!(calls.diarize.load(Ordering::SeqCst), 0);
    assert_eq!(meeting.participants(), [SpeakerId(ME.into())]);
    let rows = Index::open(&f.config.index_path())
        .unwrap()
        .list(&ListFilter::default())
        .unwrap();
    assert_eq!(rows[0].participants, [speakers.me().name.clone()]);
}

#[tokio::test]
async fn silent_meeting_is_done_with_no_segments() {
    // Silence on both channels, so the fake VAD finds no regions.
    let f = fixture_with("aevalsrc=0|0:s=48000:d=2");
    let calls = Calls::default();
    let mut engines = engines(&calls, false);
    let mut speakers = Speakers::default();

    run_from(
        &mut engines,
        &f.config,
        &f.dir,
        &mut speakers,
        Step::Segment,
        |_, _, _| {},
    )
    .await
    .unwrap();

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    for step in Step::ALL {
        done_at(&meeting, step);
    }
    assert!(meeting.segments.is_empty());
    assert_eq!(calls.llm.load(Ordering::SeqCst), 0);
    let summary = meeting.summary.expect("summary");
    assert!(summary.one_line.is_empty() && summary.body.is_empty());
    let md = std::fs::read_to_string(f.dir.markdown()).unwrap();
    assert!(
        !md.contains("## 摘要") && !md.contains("## Summary"),
        "{md}"
    );
    let rows = Index::open(&f.config.index_path())
        .unwrap()
        .list(&ListFilter::default())
        .unwrap();
    assert_eq!(rows[0].state, "done");
}

/// Marks every step before Name done and gives the meeting one remote
/// cluster that sounds like `[1, 0, 0]` and says one 40 s line, plus a line
/// from the microphone.
fn diarized(f: &Fixture) {
    let mut meeting = Meeting::load(&f.dir.path()).unwrap();
    for step in [Step::Segment, Step::Transcribe, Step::Diarize] {
        meeting
            .steps
            .insert(step, StepState::Done { at: Utc::now() });
    }
    let line = |start_ms, end_ms, side, speaker: &str| Segment {
        start_ms,
        end_ms,
        side,
        speaker: Some(SpeakerId(speaker.into())),
        text: "hello".into(),
    };
    meeting.segments = vec![
        line(0, 40_000, Side::Them, "cluster:0"),
        line(40_000, 45_000, Side::Me, ME),
    ];
    meeting.clusters = vec![Cluster {
        label: "Remote 1".into(),
        embedding: vec![1.0, 0.0, 0.0],
        speaker: None,
        named_by: None,
    }];
    meeting.save(&f.dir.path()).unwrap();
}

async fn run_name(f: &Fixture, speakers: &mut Speakers) {
    let calls = Calls::default();
    run_from(
        &mut engines(&calls, false),
        &f.config,
        &f.dir,
        speakers,
        Step::Name,
        |_, _, _| {},
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn meet_overrides_voiceprint_and_enrols_embedding() {
    let f = fixture();
    diarized(&f);
    let mut speakers = Speakers::default();
    let amy = speakers.add("Amy", Some(vec![1.0, 0.0, 0.0]));
    // Matched case-insensitively against Meet's "Tom".
    let tom = speakers.add("tom", Some(vec![0.0, 1.0, 0.0]));
    let event = |t, kind| MeetEvent { t, kind };
    append_events(
        &f.dir.meet_events(),
        &[
            event(
                0,
                MeetEventKind::Roster {
                    names: vec!["Tom".into(), "Amy".into()],
                },
            ),
            event(
                0,
                MeetEventKind::Name {
                    participant: "csrc:1".into(),
                    name: "Tom".into(),
                },
            ),
            event(
                0,
                MeetEventKind::SpeakingStart {
                    participant: "csrc:1".into(),
                },
            ),
            event(
                40_000,
                MeetEventKind::SpeakingStop {
                    participant: "csrc:1".into(),
                },
            ),
        ],
    )
    .unwrap();

    run_name(&f, &mut speakers).await;

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    assert_eq!(meeting.clusters[0].speaker, Some(tom.clone()));
    assert_eq!(meeting.clusters[0].named_by, Some(NamedBy::Meet));
    assert_eq!(meeting.segments[0].speaker, Some(tom.clone()));
    assert_eq!(meeting.participants_hint, ["Tom", "Amy"]);
    let voices = |id| speakers.get(id).unwrap().embeddings.clone();
    assert_eq!(voices(&tom), [vec![0.0, 1.0, 0.0], vec![1.0, 0.0, 0.0]]);
    assert_eq!(voices(&amy), [vec![1.0, 0.0, 0.0]]);
    assert_eq!(speakers.people().len(), 3, "no one else registered");

    // Naming again neither changes the result nor enrols the voice twice.
    run_name(&f, &mut speakers).await;
    let again = Meeting::load(&f.dir.path()).unwrap();
    assert_eq!(again.clusters, meeting.clusters);
    assert_eq!(
        speakers.get(&tom).unwrap().embeddings,
        [vec![0.0, 1.0, 0.0], vec![1.0, 0.0, 0.0]]
    );
}

#[tokio::test]
async fn meet_name_new_to_the_registry_becomes_a_person() {
    let f = fixture();
    diarized(&f);
    let mut speakers = Speakers::default();
    let event = |t, kind| MeetEvent { t, kind };
    append_events(
        &f.dir.meet_events(),
        &[
            event(
                0,
                MeetEventKind::Name {
                    participant: "csrc:1".into(),
                    name: "Tom".into(),
                },
            ),
            event(
                0,
                MeetEventKind::SpeakingStart {
                    participant: "csrc:1".into(),
                },
            ),
            event(
                40_000,
                MeetEventKind::SpeakingStop {
                    participant: "csrc:1".into(),
                },
            ),
        ],
    )
    .unwrap();

    run_name(&f, &mut speakers).await;

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    let id = meeting.clusters[0].speaker.clone().unwrap();
    let person = speakers.get(&id).unwrap();
    assert_eq!(person.name, "Tom");
    assert!(person.named);
    assert_eq!(person.embeddings, [vec![1.0, 0.0, 0.0]]);
    assert_eq!(meeting.clusters[0].named_by, Some(NamedBy::Meet));
    let saved = Speakers::load(&f.config.speakers_path()).unwrap();
    assert_eq!(saved.get(&id).unwrap().name, "Tom");
}

#[tokio::test]
async fn no_events_file_keeps_old_behaviour() {
    let f = fixture();
    diarized(&f);
    let mut speakers = Speakers::default();
    let amy = speakers.add("Amy", Some(vec![1.0, 0.0, 0.0]));

    run_name(&f, &mut speakers).await;

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    assert_eq!(meeting.clusters[0].speaker, Some(amy.clone()));
    assert_eq!(meeting.clusters[0].named_by, Some(NamedBy::Voiceprint));
    assert_eq!(meeting.segments[0].speaker, Some(amy.clone()));
    assert!(meeting.participants_hint.is_empty());
    assert_eq!(
        speakers.get(&amy).unwrap().embeddings,
        [vec![1.0, 0.0, 0.0]]
    );
    assert_eq!(speakers.people().len(), 2);
}

/// The queue worker runs pipelines on a spawned task, which needs a `Send`
/// future.
#[allow(dead_code)]
fn run_from_future_is_send(
    engines: &mut Engines,
    config: &Config,
    dir: &MeetingDir,
    speakers: &mut Speakers,
) {
    fn assert_send<T: Send>(_: &T) {}
    let future = run_from(engines, config, dir, speakers, Step::Segment, |_, _, _| {});
    assert_send(&future);
}
