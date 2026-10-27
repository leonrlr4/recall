//! The worker with fake engines, and the startup sweep.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use async_trait::async_trait;
use chrono::{Local, Utc};
use recall::worker::{INTERRUPTED, Worker, sweep_interrupted};
use recall_llm::Llm;
use recall_pipeline::runner::Engines;
use recall_pipeline::{Diarizer, Region, Transcriber, Turn, Utterance, Vad};
use recall_store::{
    Config, Event, Index, ListFilter, Meeting, MeetingDir, Speakers, Step, StepState,
};
use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc};

/// The whole channel is one region when anything in it is loud.
struct FakeVad;

impl Vad for FakeVad {
    fn regions(&self, samples16k: &[f32]) -> Result<Vec<Region>> {
        if !samples16k.iter().any(|s| s.abs() > 0.1) {
            return Ok(Vec::new());
        }
        Ok(vec![Region {
            start_ms: 0,
            end_ms: u32::try_from(samples16k.len() / 16).unwrap(),
        }])
    }
}

struct FakeTranscriber;

impl Transcriber for FakeTranscriber {
    fn transcribe(&mut self, _: &[f32], regions: &[Region], _: &str) -> Result<Vec<Utterance>> {
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

/// One new remote voice for the whole recording.
struct FakeDiarizer;

impl Diarizer for FakeDiarizer {
    fn diarize(&mut self, samples16k: &[f32]) -> Result<(Vec<Turn>, Vec<Vec<f32>>)> {
        let end_ms = u32::try_from(samples16k.len() / 16).unwrap();
        Ok((
            vec![Turn {
                start_ms: 0,
                end_ms,
                cluster: 0,
            }],
            vec![vec![0.0, 1.0, 0.0]],
        ))
    }
}

struct FakeLlm {
    fail: bool,
}

#[async_trait]
impl Llm for FakeLlm {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        if self.fail {
            bail!("model unreachable");
        }
        Ok(
            r#"{"one_line":"Said hello.","body":"","decisions":[],"actions":[],"chapters":[]}"#
                .into(),
        )
    }

    async fn stream(&self, system: &str, user: &str, tx: mpsc::Sender<String>) -> Result<()> {
        let _ = tx.send(self.complete(system, user).await?).await;
        Ok(())
    }

    fn name(&self) -> String {
        "fake/llm".into()
    }
}

struct Fixture {
    _tmp: TempDir,
    config: Arc<Config>,
    dir: MeetingDir,
    index: Arc<Mutex<Index>>,
    speakers: Arc<Mutex<Speakers>>,
    events: broadcast::Sender<Event>,
}

/// A queued 2 s meeting: a tone on the mic for the first second and on the
/// system audio for the second.
fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let config = Arc::new(Config {
        meetings_dir: tmp.path().join("meetings"),
        data_dir: tmp.path().join("data"),
        ..Config::default()
    });
    let dir = MeetingDir::create(&config.meetings_dir, Local::now(), "Worker test").unwrap();
    write_audio(&dir.audio());
    let meeting = Meeting {
        id: dir.id.clone(),
        title: "Worker test".into(),
        started: Utc::now(),
        duration_ms: 2_000,
        source_app: None,
        steps: Step::ALL.iter().map(|s| (*s, StepState::Pending)).collect(),
        segments: Vec::new(),
        clusters: Vec::new(),
        summary: None,
        models: BTreeMap::new(),
    };
    meeting.save(&dir.path()).unwrap();
    let speakers = Speakers::default();
    let index = Index::open(&config.index_path()).unwrap();
    index.upsert(&meeting, &speakers, true).unwrap();
    index.enqueue(&dir.id, Step::Segment).unwrap();
    let (events, _) = broadcast::channel(64);
    Fixture {
        _tmp: tmp,
        config,
        dir,
        index: Arc::new(Mutex::new(index)),
        speakers: Arc::new(Mutex::new(speakers)),
        events,
    }
}

fn write_audio(path: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(
            "aevalsrc=if(lt(t\\,1)\\,0.5*sin(2*PI*440*t)\\,0)\
             |if(gte(t\\,1)\\,0.5*sin(2*PI*440*t)\\,0):s=48000:d=2",
        )
        .args(["-c:a", "libopus", "-b:a", "64k"])
        .arg(path)
        .status()
        .expect("running ffmpeg");
    assert!(status.success());
}

fn worker(f: &Fixture, llm_fails: bool) -> Worker {
    Worker {
        config: f.config.clone(),
        index: f.index.clone(),
        speakers: f.speakers.clone(),
        events: f.events.clone(),
        load: Box::new(move |_| {
            Box::pin(async move {
                Ok(Engines {
                    vad: Box::new(FakeVad),
                    transcriber: Box::new(FakeTranscriber),
                    diarizer: Box::new(FakeDiarizer),
                    llm: Arc::new(FakeLlm { fail: llm_fails }),
                })
            })
        }),
        notifications: false,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn drain(rx: &mut broadcast::Receiver<Event>) -> Vec<Event> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[test]
fn job_runs_to_done_and_merges_new_speakers() {
    let f = fixture();
    let mut rx = f.events.subscribe();
    let mut worker = worker(&f, false);
    let mut engines = None;

    assert!(worker.run_next(&runtime(), &mut engines).unwrap());
    assert!(!worker.run_next(&runtime(), &mut engines).unwrap());

    let events = drain(&mut rx);
    let steps: Vec<Step> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress { meeting, step, .. } => {
                assert_eq!(meeting, &f.dir.id);
                Some(*step)
            }
            _ => None,
        })
        .collect();
    assert_eq!(steps, Step::ALL);
    assert!(
        matches!(events.last(), Some(Event::Done { meeting }) if meeting == &f.dir.id),
        "{events:?}"
    );

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    assert!(
        Step::ALL
            .iter()
            .all(|s| matches!(meeting.steps[s], StepState::Done { .. }))
    );
    let index = f.index.lock().unwrap();
    assert_eq!(index.active_job(&f.dir.id).unwrap(), None);
    assert_eq!(index.list(&ListFilter::default()).unwrap()[0].state, "done");

    // The Name step registered the remote voice; the shared registry has it.
    let remote = meeting
        .clusters
        .first()
        .and_then(|c| c.speaker.clone())
        .expect("a named cluster");
    let speakers = f.speakers.lock().unwrap();
    assert!(speakers.get(&remote).is_some(), "{speakers:?}");
    assert_eq!(speakers.people().len(), 2);
}

#[test]
fn failed_job_reports_its_step() {
    let f = fixture();
    let mut rx = f.events.subscribe();
    let mut worker = worker(&f, true);

    assert!(worker.run_next(&runtime(), &mut None).unwrap());

    let events = drain(&mut rx);
    match events.last() {
        Some(Event::Failed {
            meeting,
            step,
            error,
        }) => {
            assert_eq!(meeting, &f.dir.id);
            assert_eq!(*step, Step::Summarize);
            assert!(error.contains("model unreachable"), "{error}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert_eq!(f.index.lock().unwrap().active_job(&f.dir.id).unwrap(), None);
}

#[test]
fn worker_thread_runs_queued_jobs_and_stops_with_its_senders() {
    let f = fixture();
    let mut rx = f.events.subscribe();
    let (wake, woken) = std::sync::mpsc::channel();
    let handle = worker(&f, false).spawn(woken).unwrap();
    let done = loop {
        match rx.blocking_recv().unwrap() {
            Event::Done { meeting } => break meeting,
            Event::Failed { error, .. } => panic!("{error}"),
            _ => {}
        }
    };
    assert_eq!(done, f.dir.id);
    drop(wake);
    handle.join().unwrap();
}

#[test]
fn sweep_fails_steps_left_running() {
    let f = fixture();
    let mut meeting = Meeting::load(&f.dir.path()).unwrap();
    meeting
        .steps
        .insert(Step::Segment, StepState::Done { at: Utc::now() });
    meeting.steps.insert(Step::Transcribe, StepState::Running);
    meeting.save(&f.dir.path()).unwrap();

    let index = f.index.lock().unwrap();
    let swept = sweep_interrupted(&f.config, &index, &Speakers::default()).unwrap();

    assert_eq!(swept, std::slice::from_ref(&f.dir.id));
    let meeting = Meeting::load(&f.dir.path()).unwrap();
    match &meeting.steps[&Step::Transcribe] {
        StepState::Failed { error, .. } => assert_eq!(error, INTERRUPTED),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        meeting.steps[&Step::Segment],
        StepState::Done { .. }
    ));
    assert_eq!(meeting.steps[&Step::Diarize], StepState::Pending);
    assert_eq!(
        index.list(&ListFilter::default()).unwrap()[0].state,
        "failed:transcribe"
    );
    // Nothing left running: a second sweep changes nothing.
    assert!(
        sweep_interrupted(&f.config, &index, &Speakers::default())
            .unwrap()
            .is_empty()
    );
}
