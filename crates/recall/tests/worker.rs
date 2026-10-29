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
use recall_store::speakers::ME;
use recall_store::{
    Config, Event, Index, ListFilter, LlmConfig, MeetEvent, MeetEventKind, Meeting, MeetingDir,
    NamedBy, SpeakerId, Speakers, Step, StepState, append_events,
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

/// One new remote voice for the whole recording. Runs `during` first, to
/// act in the middle of a run.
#[derive(Default)]
struct FakeDiarizer {
    during: Option<Box<dyn FnMut() + Send>>,
}

impl Diarizer for FakeDiarizer {
    fn diarize(&mut self, samples16k: &[f32]) -> Result<(Vec<Turn>, Vec<Vec<f32>>)> {
        if let Some(during) = &mut self.during {
            during();
        }
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

/// Panics on every call, so a run gets past Name and dies in Summarize.
struct PanickingLlm;

#[async_trait]
impl Llm for PanickingLlm {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        panic!("the model blew up");
    }

    async fn stream(&self, _: &str, _: &str, _: mpsc::Sender<String>) -> Result<()> {
        panic!("the model blew up");
    }

    fn name(&self) -> String {
        "panicking".into()
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
    fixture_lasting(2)
}

/// Like [`fixture`], `secs` long, the system audio's tone lasting from the
/// first second to the end.
fn fixture_lasting(secs: u32) -> Fixture {
    fixture_with(secs, "Worker test", |_, _| {})
}

/// Like [`fixture_lasting`], titled `title`, with the config changed by
/// `configure`, which gets the fixture's temporary directory.
fn fixture_with(secs: u32, title: &str, configure: impl FnOnce(&Path, &mut Config)) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let mut config = Config {
        meetings_dir: tmp.path().join("meetings"),
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    configure(tmp.path(), &mut config);
    let config = Arc::new(config);
    let dir = MeetingDir::create(&config.meetings_dir, Local::now(), title).unwrap();
    write_audio(&dir.audio(), secs);
    let meeting = Meeting {
        id: dir.id.clone(),
        title: title.into(),
        started: Utc::now(),
        duration_ms: secs * 1_000,
        source_app: None,
        steps: Step::ALL.iter().map(|s| (*s, StepState::Pending)).collect(),
        segments: Vec::new(),
        clusters: Vec::new(),
        summary: None,
        models: BTreeMap::new(),
        participants_hint: Vec::new(),
        screen_offset_ms: None,
        screen_error: None,
        sync: false,
        calendar: None,
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

fn write_audio(path: &Path, secs: u32) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(format!(
            "aevalsrc=if(lt(t\\,1)\\,0.5*sin(2*PI*440*t)\\,0)\
             |if(gte(t\\,1)\\,0.5*sin(2*PI*440*t)\\,0):s=48000:d={secs}"
        ))
        .args(["-c:a", "libopus", "-b:a", "64k"])
        .arg(path)
        .status()
        .expect("running ffmpeg");
    assert!(status.success());
}

fn fake_engines(llm_fails: bool) -> Engines {
    Engines {
        vad: Box::new(FakeVad),
        transcriber: Box::new(FakeTranscriber),
        diarizer: Box::new(FakeDiarizer::default()),
        llm: Arc::new(FakeLlm { fail: llm_fails }),
    }
}

fn worker(f: &Fixture, llm_fails: bool) -> Worker {
    worker_with(f, move || Ok(fake_engines(llm_fails)))
}

/// A worker whose engines come from `make`, called on each load.
fn worker_with(f: &Fixture, make: impl Fn() -> Result<Engines> + Send + Sync + 'static) -> Worker {
    let make = Arc::new(make);
    Worker {
        config: f.config.clone(),
        index: f.index.clone(),
        speakers: f.speakers.clone(),
        events: f.events.clone(),
        load: Box::new(move |_| {
            let make = make.clone();
            Box::pin(async move { make() })
        }),
        notifications: false,
        vault: None,
    }
}

fn state(f: &Fixture) -> String {
    f.index
        .lock()
        .unwrap()
        .list(&ListFilter::default())
        .unwrap()[0]
        .state
        .clone()
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
    let mut steps: Vec<Step> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress { meeting, step, .. } => {
                assert_eq!(meeting, &f.dir.id);
                Some(*step)
            }
            _ => None,
        })
        .collect();
    // Transcribe also reports each side's piece of the transcript.
    assert_eq!(steps.iter().filter(|s| **s == Step::Transcribe).count(), 3);
    steps.dedup();
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

#[test]
fn unbuildable_llm_fails_only_the_summary() {
    let f = fixture();
    let mut worker = worker_with(&f, || {
        let mut engines = fake_engines(false);
        engines.llm = recall_llm::from_config_or_unavailable(&LlmConfig {
            backend: "nonsense".into(),
            ..LlmConfig::default()
        });
        Ok(engines)
    });

    assert!(worker.run_next(&runtime(), &mut None).unwrap());

    assert_eq!(state(&f), "failed:summarize");
    let meeting = Meeting::load(&f.dir.path()).unwrap();
    assert!(!meeting.segments.is_empty());
    match &meeting.steps[&Step::Summarize] {
        StepState::Failed { error, .. } => {
            assert!(error.contains("unknown LLM backend"), "{error}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn engine_load_failure_marks_the_first_step_failed() {
    let f = fixture();
    let mut rx = f.events.subscribe();
    let mut worker = worker_with(&f, || bail!("model file is corrupt"));

    assert!(worker.run_next(&runtime(), &mut None).unwrap());

    assert_eq!(state(&f), "failed:segment");
    let meeting = Meeting::load(&f.dir.path()).unwrap();
    match &meeting.steps[&Step::Segment] {
        StepState::Failed { error, .. } => assert!(error.contains("corrupt"), "{error}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        drain(&mut rx).last(),
        Some(Event::Failed {
            step: Step::Segment,
            ..
        })
    ));
    assert_eq!(f.index.lock().unwrap().active_job(&f.dir.id).unwrap(), None);
}

#[test]
fn engine_load_panic_fails_the_job_and_the_worker_goes_on() {
    let f = fixture();
    let mut worker = worker_with(&f, || panic!("onnxruntime rejected the model"));

    assert!(worker.run_next(&runtime(), &mut None).unwrap());

    assert_eq!(state(&f), "failed:segment");
    let meeting = Meeting::load(&f.dir.path()).unwrap();
    match &meeting.steps[&Step::Segment] {
        StepState::Failed { error, .. } => {
            assert!(error.contains("onnxruntime rejected the model"), "{error}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(f.index.lock().unwrap().active_job(&f.dir.id).unwrap(), None);
    // The queue is empty now, and the worker still answers.
    assert!(!worker.run_next(&runtime(), &mut None).unwrap());
}

#[test]
fn rename_during_a_run_survives_the_speakers_merge() {
    let f = fixture();
    let shared = f.speakers.clone();
    let path = f.config.speakers_path();
    let me = SpeakerId(ME.into());
    let mut worker = worker_with(&f, move || {
        let shared = shared.clone();
        let path = path.clone();
        let mut engines = fake_engines(false);
        engines.diarizer = Box::new(FakeDiarizer {
            // What PATCH /api/people does, after the run took its copy.
            during: Some(Box::new(move || {
                let mut speakers = shared.lock().unwrap();
                speakers.rename(&SpeakerId(ME.into()), "Leon").unwrap();
                speakers.save(&path).unwrap();
            })),
        });
        Ok(engines)
    });

    assert!(worker.run_next(&runtime(), &mut None).unwrap());
    assert_eq!(state(&f), "done");

    let on_disk = Speakers::load(&f.config.speakers_path()).unwrap();
    let in_memory = f.speakers.lock().unwrap().clone();
    assert_eq!(on_disk, in_memory);
    assert_eq!(in_memory.name_of(&me), "Leon");
    assert_eq!(in_memory.people().len(), 2, "{in_memory:?}");
}

#[test]
fn run_failing_at_summarize_still_adds_its_new_voices() {
    let f = fixture();
    let mut worker = worker(&f, true);

    assert!(worker.run_next(&runtime(), &mut None).unwrap());

    assert_eq!(state(&f), "failed:summarize");
    let remote = Meeting::load(&f.dir.path()).unwrap().clusters[0]
        .speaker
        .clone()
        .unwrap();
    assert!(f.speakers.lock().unwrap().get(&remote).is_some());
    let on_disk = Speakers::load(&f.config.speakers_path()).unwrap();
    assert!(on_disk.get(&remote).is_some());
}

#[test]
fn panicking_job_is_failed_keeps_its_voices_and_the_worker_goes_on() {
    let f = fixture();
    let mut worker = worker_with(&f, || {
        let mut engines = fake_engines(false);
        engines.llm = Arc::new(PanickingLlm);
        Ok(engines)
    });
    let mut engines = None;

    assert!(worker.run_next(&runtime(), &mut engines).unwrap());

    assert!(engines.is_none(), "engines are reloaded after a panic");
    assert_eq!(state(&f), "failed:summarize");
    let meeting = Meeting::load(&f.dir.path()).unwrap();
    match &meeting.steps[&Step::Summarize] {
        StepState::Failed { error, .. } => {
            assert!(
                error.contains("pipeline panicked: the model blew up"),
                "{error}"
            )
        }
        other => panic!("{other:?}"),
    }
    // The Name step registered the remote voice before the panic.
    let remote = meeting.clusters[0].speaker.clone().unwrap();
    assert!(f.speakers.lock().unwrap().get(&remote).is_some());
    let on_disk = Speakers::load(&f.config.speakers_path()).unwrap();
    assert!(on_disk.get(&remote).is_some());
    assert_eq!(f.index.lock().unwrap().active_job(&f.dir.id).unwrap(), None);
    assert!(!worker.run_next(&runtime(), &mut engines).unwrap());
}

#[test]
fn voice_enrolled_through_meet_reaches_the_shared_registry() {
    // 40 s, so the remote voice has the 30 s of Meet speech a name needs.
    let f = fixture_lasting(40);
    let tom = f
        .speakers
        .lock()
        .unwrap()
        .add("Tom", Some(vec![1.0, 0.0, 0.0]));
    let participant = || "csrc:1".to_string();
    append_events(
        &f.dir.meet_events(),
        &[
            MeetEvent {
                t: 0,
                kind: MeetEventKind::Name {
                    participant: participant(),
                    name: "Tom".into(),
                },
            },
            MeetEvent {
                t: 0,
                kind: MeetEventKind::SpeakingStart {
                    participant: participant(),
                },
            },
            MeetEvent {
                t: 40_000,
                kind: MeetEventKind::SpeakingStop {
                    participant: participant(),
                },
            },
        ],
    )
    .unwrap();
    let mut worker = worker(&f, false);

    assert!(worker.run_next(&runtime(), &mut None).unwrap());
    assert_eq!(state(&f), "done");

    let cluster = &Meeting::load(&f.dir.path()).unwrap().clusters[0];
    assert_eq!(cluster.speaker, Some(tom.clone()));
    assert_eq!(cluster.named_by, Some(NamedBy::Meet));
    // The fake diarizer's voice is added to Tom's own.
    let voices = [vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]];
    assert_eq!(
        f.speakers.lock().unwrap().get(&tom).unwrap().embeddings,
        voices
    );
    let on_disk = Speakers::load(&f.config.speakers_path()).unwrap();
    assert_eq!(on_disk.get(&tom).unwrap().embeddings, voices);
}

/// A fake `claude` in `dir` that saves its stdin to `stdin.txt` and prints
/// a print-mode JSON result whose text is `reply`, or fails without one.
fn fake_claude(dir: &Path, reply: Option<&str>) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("claude");
    let body = match reply {
        Some(reply) => {
            let result = serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "result": reply,
            });
            std::fs::write(dir.join("reply.json"), result.to_string()).unwrap();
            format!(
                "cat > '{0}/stdin.txt'\ncat '{0}/reply.json'\n",
                dir.display()
            )
        }
        None => "echo 'not logged in' >&2\nexit 1\n".into(),
    };
    std::fs::write(&script, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// An event from two minutes before now to half an hour after.
fn calendar_reply() -> String {
    let now = Utc::now();
    let at = |minutes: i64| (now + chrono::Duration::minutes(minutes)).to_rfc3339();
    serde_json::json!([{
        "id": "ev1",
        "title": "Interns culture fit",
        "start": at(-2),
        "end": at(30),
        "all_day": false,
        "attendees": [
            {"name": "Tom Lin", "email": "tom@example.com", "response": "accepted", "self": false},
            {"name": null, "email": "me@example.com", "response": "accepted", "self": true},
        ],
    }])
    .to_string()
}

fn with_calendar(reply: Option<String>) -> impl FnOnce(&Path, &mut Config) {
    move |tmp, config| {
        config.calendar.enabled = true;
        config.claude_code.command = fake_claude(tmp, reply.as_deref())
            .to_string_lossy()
            .into_owned();
    }
}

#[test]
fn calendar_event_titles_a_default_meeting_and_names_its_attendees() {
    let default_title = format!("Meeting {}", Local::now().format("%H:%M"));
    let f = fixture_with(2, &default_title, with_calendar(Some(calendar_reply())));
    let mut rx = f.events.subscribe();
    let mut worker = worker(&f, false);

    assert!(worker.run_next(&runtime(), &mut None).unwrap());

    let stdin = std::fs::read_to_string(f._tmp.path().join("stdin.txt")).unwrap();
    assert!(stdin.contains("List the events"), "{stdin}");
    assert!(!f.dir.path().exists(), "the directory moved");
    let ids = MeetingDir::list(&f.config.meetings_dir).unwrap();
    assert_eq!(ids.len(), 1);
    let id = ids[0].clone();
    assert!(id.0.ends_with(" Interns culture fit"), "{id:?}");
    let dir = MeetingDir::open(&f.config.meetings_dir, &id).unwrap();
    let meeting = Meeting::load(&dir.path()).unwrap();
    assert_eq!(meeting.id, id);
    assert_eq!(meeting.title, "Interns culture fit");
    assert_eq!(meeting.participants_hint, ["Tom Lin"]);
    let event = meeting.calendar.as_ref().unwrap();
    assert_eq!(event.id, "ev1");
    assert_eq!(event.attendees[0].email, "tom@example.com");
    let md = std::fs::read_to_string(dir.markdown()).unwrap();
    assert!(md.contains("attendees: [Tom Lin]\n"), "{md}");
    assert!(!md.contains('@'), "{md}");

    let events = drain(&mut rx);
    assert!(
        matches!(events.last(), Some(Event::Done { meeting }) if meeting == &id),
        "{events:?}"
    );
    let index = f.index.lock().unwrap();
    let rows = index.list(&ListFilter::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].state, "done");
    assert_eq!(index.active_job(&f.dir.id).unwrap(), None);
}

#[test]
fn calendar_event_keeps_a_title_the_user_typed() {
    let f = fixture_with(2, "Worker test", with_calendar(Some(calendar_reply())));
    let mut worker = worker(&f, false);

    assert!(worker.run_next(&runtime(), &mut None).unwrap());

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    assert_eq!(meeting.title, "Worker test");
    assert_eq!(meeting.participants_hint, ["Tom Lin"]);
    assert!(meeting.calendar.is_some());
    assert_eq!(state(&f), "done");
}

#[test]
fn failed_calendar_lookup_changes_nothing() {
    let default_title = format!("Meeting {}", Local::now().format("%H:%M"));
    let f = fixture_with(2, &default_title, with_calendar(None));
    let mut worker = worker(&f, false);

    assert!(worker.run_next(&runtime(), &mut None).unwrap());

    let meeting = Meeting::load(&f.dir.path()).unwrap();
    assert_eq!(meeting.title, default_title);
    assert_eq!(meeting.calendar, None);
    assert!(meeting.participants_hint.is_empty());
    assert_eq!(state(&f), "done");
}
