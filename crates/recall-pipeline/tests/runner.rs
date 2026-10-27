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
    Config, Index, ListFilter, Meeting, MeetingDir, Side, SpeakerId, Speakers, Step, StepState,
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
        |step, pct| progress.lock().unwrap().push((step, pct)),
    )
    .await
    .unwrap();

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    for step in Step::ALL {
        done_at(&meeting, step);
    }
    let progress = progress.into_inner().unwrap();
    assert_eq!(
        progress.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        Step::ALL
    );
    assert!((progress.last().unwrap().1 - 1.0).abs() < 1e-6);

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
        |_, _| {},
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
        |step, pct| progress.lock().unwrap().push((step, pct)),
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
    assert_eq!(progress, [(Step::Summarize, 5.0 / 6.0), (Step::Write, 1.0)]);
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
        |_, _| {},
    )
    .await
    .unwrap();
    assert_eq!(calls.llm.load(Ordering::SeqCst), 3);
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
        |_, _| {},
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
        |_, _| {},
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
        |_, _| {},
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
    let future = run_from(engines, config, dir, speakers, Step::Segment, |_, _| {});
    assert_send(&future);
}
