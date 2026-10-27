use std::path::Path;

use chrono::{Local, TimeZone};
use recall::recorder::Recorder;
use recall_audio::writer::{ChunkWriter, Track};
use recall_store::{Config, Index, ListFilter, Meeting, MeetingDir, Speakers, Step, StepState};

/// A meeting directory without `meeting.json` whose cache holds `secs`
/// seconds of silence on both tracks, as a crash mid-recording leaves it.
fn crashed_recording(root: &Path, secs: usize) -> MeetingDir {
    let started = Local.with_ymd_and_hms(2026, 10, 27, 14, 30, 0).unwrap();
    let dir = MeetingDir::create(root, started, "Meeting 14:30").unwrap();
    let mut writer = ChunkWriter::new(&dir.cache()).unwrap();
    let silence = vec![0.0f32; secs * 48_000];
    writer.push(Track::Mic, &silence).unwrap();
    writer.push(Track::Sys, &silence).unwrap();
    writer.flush().unwrap();
    assert!(dir.has_unfinished_cache());
    dir
}

fn config(root: &Path, min_length_secs: u64) -> Config {
    Config {
        meetings_dir: root.to_path_buf(),
        data_dir: root.join("data"),
        min_length_secs,
        ..Config::default()
    }
}

#[test]
fn recovery_finalizes_unfinished_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("meetings");
    let dir = crashed_recording(&root, 3);
    let config = config(&root, 1);
    let index = Index::open(&tmp.path().join("index.sqlite")).unwrap();

    let ids = Recorder::recover(&config, &index, &Speakers::default()).unwrap();

    assert_eq!(ids, vec![dir.id.clone()]);
    assert!(dir.audio().is_file());
    assert!(!dir.cache().exists());
    let meeting = Meeting::load(&dir.path()).unwrap();
    assert_eq!(meeting.id, dir.id);
    assert_eq!(meeting.title, "Meeting 14:30");
    assert_eq!(meeting.duration_ms, 3_000);
    assert_eq!(
        meeting.started,
        Local
            .with_ymd_and_hms(2026, 10, 27, 14, 30, 0)
            .unwrap()
            .to_utc()
    );
    assert!(
        Step::ALL
            .iter()
            .all(|s| meeting.steps.get(s) == Some(&StepState::Pending))
    );
    let rows = index.list(&ListFilter::default()).unwrap();
    assert_eq!(rows.len(), 1);
    let job = index.next_job().unwrap().expect("a job is queued");
    assert_eq!(job.meeting, dir.id);
    assert_eq!(job.from_step, Step::Segment);
    assert!(index.next_job().unwrap().is_none());
}

#[test]
fn recovery_deletes_short_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("meetings");
    let dir = crashed_recording(&root, 30);
    let config = config(&root, 120);
    let index = Index::open(&tmp.path().join("index.sqlite")).unwrap();

    let ids = Recorder::recover(&config, &index, &Speakers::default()).unwrap();

    assert!(ids.is_empty());
    assert!(!dir.path().exists());
    assert!(index.next_job().unwrap().is_none());
}

#[test]
fn recovery_leaves_finished_meetings_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("meetings");
    let dir = crashed_recording(&root, 3);
    let config = config(&root, 1);
    let index = Index::open(&tmp.path().join("index.sqlite")).unwrap();
    Recorder::recover(&config, &index, &Speakers::default()).unwrap();
    index.next_job().unwrap();

    let again = Recorder::recover(&config, &index, &Speakers::default()).unwrap();

    assert!(again.is_empty());
    assert!(dir.audio().is_file());
    assert!(index.next_job().unwrap().is_none());
}

#[test]
fn recovery_after_save_only_clears_and_queues() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("meetings");
    let dir = crashed_recording(&root, 3);
    let config = config(&root, 1);
    let index = Index::open(&tmp.path().join("index.sqlite")).unwrap();
    // A crash between saving meeting.json and clearing the cache.
    std::fs::write(dir.audio(), b"encoded").unwrap();
    let meeting = Meeting {
        id: dir.id.clone(),
        title: "Meeting 14:30".into(),
        started: chrono::Utc::now(),
        duration_ms: 3_000,
        source_app: Some("brave".into()),
        steps: Step::ALL.map(|s| (s, StepState::Pending)).into(),
        segments: vec![],
        clusters: vec![],
        summary: None,
        models: Default::default(),
    };
    meeting.save(&dir.path()).unwrap();

    let ids = Recorder::recover(&config, &index, &Speakers::default()).unwrap();

    assert_eq!(ids, vec![dir.id.clone()]);
    assert!(!dir.cache().exists());
    assert_eq!(std::fs::read(dir.audio()).unwrap(), b"encoded");
    assert_eq!(Meeting::load(&dir.path()).unwrap(), meeting);
    assert_eq!(index.next_job().unwrap().unwrap().meeting, dir.id);
}

#[test]
fn recovery_does_not_queue_a_second_job() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("meetings");
    let dir = crashed_recording(&root, 3);
    let config = config(&root, 1);
    let index = Index::open(&tmp.path().join("index.sqlite")).unwrap();
    // A crash after enqueue, before the cache was cleared.
    std::fs::write(dir.audio(), b"encoded").unwrap();
    let meeting = Meeting {
        id: dir.id.clone(),
        title: "Meeting 14:30".into(),
        started: chrono::Utc::now(),
        duration_ms: 3_000,
        source_app: None,
        steps: Step::ALL.map(|s| (s, StepState::Pending)).into(),
        segments: vec![],
        clusters: vec![],
        summary: None,
        models: Default::default(),
    };
    meeting.save(&dir.path()).unwrap();
    let job = index.enqueue(&dir.id, Step::Segment).unwrap();

    let ids = Recorder::recover(&config, &index, &Speakers::default()).unwrap();

    assert_eq!(ids, vec![dir.id.clone()]);
    assert!(!dir.cache().exists());
    assert_eq!(index.next_job().unwrap().unwrap().id, job);
    assert!(index.next_job().unwrap().is_none());
}
