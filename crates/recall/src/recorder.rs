//! Turns the session's [`Action`]s into captures, meeting directories and
//! queued jobs, and recovers recordings a crash left in their cache.
//!
//! Keeping a recording runs: encode `audio.opus`, write `meeting.json`,
//! index, enqueue, clear `.cache/`. It runs on a thread of its own per
//! recording, so a long encode never holds up the next recording; one that
//! fails is retried every [`KEEP_RETRY_DELAY`]. While `.cache/` holds
//! chunks, recovery redoes the recording: `meeting.json` is written only
//! after the audio is complete, so its presence means only indexing and
//! queueing are left, and both are idempotent ([`Index::enqueue`] returns
//! the existing job).

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, Utc};
use recall_audio::capture::{Capture, Levels};
use recall_audio::state::Action;
use recall_audio::writer::{ChunkWriter, cache_duration, finalize};
use recall_store::{Config, Index, Meeting, MeetingDir, MeetingId, Speakers, Step, StepState};

use crate::notify;

/// `"YYYY-MM-DD HH:MM"`, the start of every meeting directory name.
const ID_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";
const ID_TIME_LEN: usize = 16;
/// How long after a failed keep (a full disk, say) it is tried again.
pub const KEEP_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Free space in `meetings_dir` below which starting a recording warns.
const LOW_SPACE_BYTES: u64 = 2_000_000_000;

/// What the recorder needs from a running capture: [`Capture`] in the
/// daemon, a fake in tests.
pub trait CaptureControl: Send {
    /// False once the capture has ended on its own.
    fn is_running(&self) -> bool;
    /// Stops it and returns the first error it hit.
    fn stop(self: Box<Self>) -> Result<()>;
}

impl CaptureControl for Capture {
    fn is_running(&self) -> bool {
        Capture::is_running(self)
    }

    fn stop(self: Box<Self>) -> Result<()> {
        Capture::stop(*self)
    }
}

/// Starts capturing into the cache directory given.
pub type StartCapture = dyn Fn(&Path, Arc<Levels>) -> Result<Box<dyn CaptureControl>> + Send + Sync;
/// Encodes a cache into an audio file; [`finalize`] in the daemon.
pub type Finalize = dyn Fn(&Path, &Path) -> Result<Duration> + Send + Sync;

/// A recording, in progress or being kept. Its capture is
/// [`Recorder::capture`] while it records.
#[derive(Clone)]
struct Active {
    dir: MeetingDir,
    title: String,
    started: DateTime<Local>,
    source_app: Option<String>,
    /// Started with the hotkey, so kept whatever its length.
    by_hotkey: bool,
}

/// A finished recording being kept on its own thread.
struct Keeping {
    active: Active,
    /// Earlier attempts that failed.
    failures: u32,
    thread: JoinHandle<Result<Option<Meeting>>>,
}

/// A recording whose keep failed, to be tried again at `retry_at`.
struct FailedKeep {
    active: Active,
    failures: u32,
    retry_at: Instant,
}

pub struct Recorder {
    config: Arc<Config>,
    index: Arc<Mutex<Index>>,
    speakers: Arc<Mutex<Speakers>>,
    levels: Arc<Levels>,
    active: Option<Active>,
    /// Set exactly when `active` is.
    capture: Option<Box<dyn CaptureControl>>,
    /// Told after a kept recording is queued.
    wake: Option<Sender<()>>,
    start_capture: Arc<StartCapture>,
    finalize: Arc<Finalize>,
    notifications: bool,
    keeping: Vec<Keeping>,
    failed: Vec<FailedKeep>,
}

impl Recorder {
    pub fn new(
        config: Arc<Config>,
        index: Arc<Mutex<Index>>,
        speakers: Arc<Mutex<Speakers>>,
    ) -> Self {
        Self {
            config,
            index,
            speakers,
            levels: Arc::new(Levels::default()),
            active: None,
            capture: None,
            wake: None,
            start_capture: Arc::new(|cache: &Path, levels: Arc<Levels>| {
                let writer = ChunkWriter::new(cache)?;
                let capture = Capture::start(Arc::new(Mutex::new(writer)), levels)?;
                Ok(Box::new(capture) as Box<dyn CaptureControl>)
            }),
            finalize: Arc::new(|cache: &Path, out: &Path| finalize(cache, out)),
            notifications: true,
            keeping: Vec::new(),
            failed: Vec::new(),
        }
    }

    /// Replaces how captures start and caches are encoded, and turns
    /// desktop notifications off; for tests.
    pub fn with_fakes(mut self, start_capture: Arc<StartCapture>, finalize: Arc<Finalize>) -> Self {
        self.start_capture = start_capture;
        self.finalize = finalize;
        self.notifications = false;
        self
    }

    /// Sends `()` on `wake` whenever a finished recording is queued, so the
    /// worker starts on it without waiting for its next poll.
    pub fn with_wake(mut self, wake: Sender<()>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Carries out `action`, taken at `now`.
    ///
    /// - `StartCache` creates `<meetings_dir>/<now> Meeting HH:MM/` and starts
    ///   capturing into its `.cache/`.
    /// - `StartRecording` remembers the source app and notifies. A hotkey
    ///   start from idle comes without `StartCache`, so it starts the capture
    ///   first.
    /// - `Finish` stops the capture and starts keeping the recording on a
    ///   thread of its own (see [`Recorder::poll_keeps`]), or deletes it if
    ///   it is shorter than `min_length_secs`. A recording started with the
    ///   hotkey is kept whatever its length.
    /// - `Discard` stops the capture and deletes the directory.
    pub fn apply(&mut self, action: Action, now: DateTime<Local>) -> Result<()> {
        match action {
            Action::None => Ok(()),
            Action::StartCache => {
                if self.active.is_some() {
                    tracing::warn!("asked to start a cache while capturing; ignored");
                    return Ok(());
                }
                self.start(now)
            }
            Action::StartRecording {
                source_app,
                by_hotkey,
            } => {
                if self.active.is_none() {
                    self.start(now)?;
                }
                tracing::info!(source_app, by_hotkey, "recording");
                if self.notifications {
                    notify::recording_started(&source_app);
                }
                if let Some(active) = &mut self.active {
                    active.source_app = Some(source_app);
                    active.by_hotkey = by_hotkey;
                }
                Ok(())
            }
            Action::Finish => {
                if let Some(active) = self.stop() {
                    self.spawn_keep(active, 0);
                }
                Ok(())
            }
            Action::Discard => {
                let Some(active) = self.stop() else {
                    return Ok(());
                };
                tracing::info!(id = active.dir.id.0, "recording discarded");
                remove_dir(&active.dir.path())
            }
        }
    }

    /// Handles keeps that have finished: a failed one is logged, notified on
    /// its first failure, and scheduled again [`KEEP_RETRY_DELAY`] after
    /// `now`. Starts the retries that are due at `now`. Never blocks.
    pub fn poll_keeps(&mut self, now: Instant) {
        let (done, running): (Vec<_>, Vec<_>) = std::mem::take(&mut self.keeping)
            .into_iter()
            .partition(|k| k.thread.is_finished());
        self.keeping = running;
        for keeping in done {
            self.keep_finished(keeping, now);
        }
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.failed)
            .into_iter()
            .partition(|f| f.retry_at <= now);
        self.failed = waiting;
        for failed in due {
            tracing::info!(id = failed.active.dir.id.0, "retrying to keep a recording");
            self.spawn_keep(failed.active, failed.failures);
        }
    }

    /// Waits for every keep in progress, as the daemon does before it exits.
    /// Recordings that fail stay in their cache for recovery on the next
    /// start.
    pub fn wait_keeps(&mut self) {
        for keeping in std::mem::take(&mut self.keeping) {
            self.keep_finished(keeping, Instant::now());
        }
    }

    /// Recordings being kept or waiting to be retried.
    pub fn keeps_pending(&self) -> usize {
        self.keeping.len() + self.failed.len()
    }

    /// Starts keeping `active` on its own thread. The encode runs without
    /// the locks, which the HTTP server also takes; they are held only to
    /// index and queue.
    fn spawn_keep(&mut self, active: Active, failures: u32) {
        let config = self.config.clone();
        let index = self.index.clone();
        let speakers = self.speakers.clone();
        let finalize = self.finalize.clone();
        let wake = self.wake.clone();
        let notifications = self.notifications;
        let job = active.clone();
        let run = move || {
            let min_length = if job.by_hotkey {
                Duration::ZERO
            } else {
                Duration::from_secs(config.min_length_secs)
            };
            let kept = keep(
                &job.dir,
                &job.title,
                job.started.to_utc(),
                job.source_app.clone(),
                min_length,
                finalize.as_ref(),
                |meeting| {
                    let speakers = lock(&speakers);
                    let index = lock(&index);
                    queue(&index, &speakers, meeting)
                },
            )?;
            match &kept {
                Some(meeting) => {
                    tracing::info!(id = meeting.id.0, "recording kept");
                    if notifications {
                        notify::processing(&meeting.title);
                    }
                    if let Some(wake) = &wake {
                        let _ = wake.send(());
                    }
                }
                None => tracing::info!(id = job.dir.id.0, "recording too short; deleted"),
            }
            Ok(kept)
        };
        let thread = std::thread::Builder::new()
            .name("recall-keep".into())
            .spawn(run)
            .unwrap_or_else(|err| {
                tracing::error!(%err, "spawning a keep thread; retrying later");
                std::thread::spawn(|| Err(anyhow::anyhow!("no thread to keep the recording on")))
            });
        self.keeping.push(Keeping {
            active,
            failures,
            thread,
        });
    }

    /// Joins a finished (or, from [`Recorder::wait_keeps`], running) keep
    /// and schedules a retry if it failed.
    fn keep_finished(&mut self, keeping: Keeping, now: Instant) {
        let Keeping {
            active,
            failures,
            thread,
        } = keeping;
        let err = match thread.join() {
            Ok(Ok(_)) => return,
            Ok(Err(err)) => err,
            Err(_) => anyhow::anyhow!("the keep thread panicked"),
        };
        tracing::error!(
            err = format!("{err:#}"),
            id = active.dir.id.0,
            failures = failures + 1,
            "keeping a recording failed; retrying in {} s",
            KEEP_RETRY_DELAY.as_secs()
        );
        if failures == 0 && self.notifications {
            notify::failed(&active.title, "saving");
        }
        self.failed.push(FailedKeep {
            active,
            failures: failures + 1,
            retry_at: now + KEEP_RETRY_DELAY,
        });
    }

    /// Whether a capture was started and not yet stopped.
    pub fn is_capturing(&self) -> bool {
        self.active.is_some()
    }

    /// True when a capture was started and has since ended on its own.
    pub fn capture_failed(&self) -> bool {
        self.capture
            .as_ref()
            .is_some_and(|capture| !capture.is_running())
    }

    /// Peak levels of the latest mic and system buffers, 0 when idle.
    pub fn peaks(&self) -> (f32, f32) {
        if self.active.is_none() {
            return (0.0, 0.0);
        }
        (
            f32::from_bits(self.levels.mic.load(Ordering::Relaxed)),
            f32::from_bits(self.levels.sys.load(Ordering::Relaxed)),
        )
    }

    pub fn source_app(&self) -> Option<&str> {
        self.active.as_ref()?.source_app.as_deref()
    }

    /// When the current capture started.
    pub fn started(&self) -> Option<DateTime<Local>> {
        self.active.as_ref().map(|active| active.started)
    }

    fn start(&mut self, now: DateTime<Local>) -> Result<()> {
        let title = format!("Meeting {}", now.format("%H:%M"));
        let dir = MeetingDir::create(&self.config.meetings_dir, now, &title)?;
        self.warn_if_low_on_space();
        self.levels.mic.store(0, Ordering::Relaxed);
        self.levels.sys.store(0, Ordering::Relaxed);
        let capture = (self.start_capture)(&dir.cache(), self.levels.clone());
        let capture = match capture {
            Ok(capture) => capture,
            Err(err) => {
                if let Err(rm) = remove_dir(&dir.path()) {
                    tracing::warn!(
                        err = format!("{rm:#}"),
                        "removing an unused meeting directory"
                    );
                }
                return Err(err.context("starting the capture"));
            }
        };
        tracing::info!(id = dir.id.0, "capturing");
        self.capture = Some(capture);
        self.active = Some(Active {
            dir,
            title,
            started: now,
            source_app: None,
            by_hotkey: false,
        });
        Ok(())
    }

    /// Logs and notifies when `meetings_dir` has less than 2 GB free, about
    /// an hour of cache for both tracks.
    fn warn_if_low_on_space(&self) {
        let dir = &self.config.meetings_dir;
        let free = match rustix::fs::statvfs(dir) {
            Ok(stat) => stat.f_bavail.saturating_mul(stat.f_frsize),
            Err(err) => {
                tracing::warn!(%err, dir = %dir.display(), "reading free space");
                return;
            }
        };
        if free < LOW_SPACE_BYTES {
            tracing::warn!(free, dir = %dir.display(), "low on disk space");
            if self.notifications {
                notify::low_space(dir, free);
            }
        }
    }

    /// Stops the capture and returns what it was recording. A capture error
    /// is logged, not returned: the chunks already on disk are still usable.
    fn stop(&mut self) -> Option<Active> {
        let active = self.active.take()?;
        if let Some(capture) = self.capture.take()
            && let Err(err) = capture.stop()
        {
            tracing::error!(
                err = format!("{err:#}"),
                id = active.dir.id.0,
                "the capture failed"
            );
        }
        Some(active)
    }

    /// Finishes every recording under `config.meetings_dir` whose `.cache/`
    /// still holds chunks, as a crash or kill leaves them: a cache shorter
    /// than `min_length_secs` is deleted with its directory, the rest is
    /// encoded, saved, indexed and queued from [`Step::Segment`]. Returns the
    /// ids queued. A directory that fails is logged and skipped.
    pub fn recover(config: &Config, index: &Index, speakers: &Speakers) -> Result<Vec<MeetingId>> {
        let root = &config.meetings_dir;
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort_unstable();
        let mut queued = Vec::new();
        for name in names {
            // Skips anything `MeetingDir::create` did not make.
            let Ok(dir) = MeetingDir::open(root, &MeetingId(name)) else {
                continue;
            };
            if !dir.has_unfinished_cache() {
                continue;
            }
            match recover_dir(config, index, speakers, &dir) {
                Ok(Some(id)) => {
                    tracing::info!(id = id.0, "recovered a recording");
                    queued.push(id);
                }
                Ok(None) => tracing::info!(id = dir.id.0, "deleted a short unfinished recording"),
                Err(err) => {
                    tracing::error!(
                        err = format!("{err:#}"),
                        id = dir.id.0,
                        "recovering a recording"
                    )
                }
            }
        }
        Ok(queued)
    }
}

/// Recovers one directory with an unfinished cache.
fn recover_dir(
    config: &Config,
    index: &Index,
    speakers: &Speakers,
    dir: &MeetingDir,
) -> Result<Option<MeetingId>> {
    let path = dir.path();
    if path.join("meeting.json").is_file() {
        // Saved, so the audio was complete: the crash came after `save`.
        let meeting = Meeting::load(&path)?;
        queue(index, speakers, &meeting)?;
        dir.clear_cache()?;
        return Ok(Some(meeting.id));
    }
    let name = &dir.id.0;
    let started = name
        .get(..ID_TIME_LEN)
        .and_then(|t| NaiveDateTime::parse_from_str(t, ID_TIME_FORMAT).ok())
        .and_then(|t| t.and_local_timezone(Local).earliest())
        .map_or_else(Utc::now, |t| t.to_utc());
    let title = name.get(ID_TIME_LEN + 1..).unwrap_or_default();
    let min_length = Duration::from_secs(config.min_length_secs);
    let kept = keep(
        dir,
        title,
        started,
        None,
        min_length,
        &finalize,
        |meeting| queue(index, speakers, meeting),
    )?;
    Ok(kept.map(|meeting| meeting.id))
}

/// Encodes the cache of `dir` with `finalize`, saves `meeting.json`, hands
/// the meeting to `queue` (index and enqueue) and clears the cache; or
/// deletes `dir` if the cache is empty or shorter than `min_length`. Returns
/// the meeting kept.
fn keep(
    dir: &MeetingDir,
    title: &str,
    started: DateTime<Utc>,
    source_app: Option<String>,
    min_length: Duration,
    finalize: &Finalize,
    queue: impl FnOnce(&Meeting) -> Result<()>,
) -> Result<Option<Meeting>> {
    let cache = dir.cache();
    let cached = cache_duration(&cache)?;
    if cached.is_zero() || cached < min_length {
        remove_dir(&dir.path())?;
        return Ok(None);
    }
    let duration = finalize(&cache, &dir.audio())?;
    let meeting = Meeting {
        id: dir.id.clone(),
        title: title.to_owned(),
        started,
        duration_ms: u32::try_from(duration.as_millis()).unwrap_or(u32::MAX),
        source_app,
        steps: Step::ALL.map(|s| (s, StepState::Pending)).into(),
        segments: Vec::new(),
        clusters: Vec::new(),
        summary: None,
        models: BTreeMap::new(),
        participants_hint: Vec::new(),
        screen_offset_ms: None,
        screen_error: None,
    };
    meeting.save(&dir.path())?;
    queue(&meeting)?;
    dir.clear_cache()?;
    Ok(Some(meeting))
}

/// Indexes `meeting` and queues its processing from the first step.
fn queue(index: &Index, speakers: &Speakers, meeting: &Meeting) -> Result<()> {
    index.upsert(meeting, speakers, true)?;
    index.enqueue(&meeting.id, Step::Segment)?;
    Ok(())
}

fn remove_dir(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    use chrono::TimeZone;
    use recall_audio::writer::Track;

    use super::*;

    /// A capture that does nothing; [`fake_start`] fills the cache.
    struct FakeCapture;

    impl CaptureControl for FakeCapture {
        fn is_running(&self) -> bool {
            true
        }

        fn stop(self: Box<Self>) -> Result<()> {
            Ok(())
        }
    }

    /// Starts a [`FakeCapture`] after writing `secs` seconds of silence to
    /// both tracks.
    fn fake_start(secs: usize) -> Arc<StartCapture> {
        Arc::new(move |cache: &Path, _| {
            let mut writer = ChunkWriter::new(cache)?;
            let silence = vec![0.0f32; secs * 48_000];
            writer.push(Track::Mic, &silence)?;
            writer.push(Track::Sys, &silence)?;
            writer.flush()?;
            Ok(Box::new(FakeCapture) as Box<dyn CaptureControl>)
        })
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        index: Arc<Mutex<Index>>,
        config: Arc<Config>,
    }

    fn fixture(min_length_secs: u64) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(Config {
            meetings_dir: tmp.path().join("meetings"),
            data_dir: tmp.path().join("data"),
            min_length_secs,
            ..Config::default()
        });
        let index = Arc::new(Mutex::new(
            Index::open(&tmp.path().join("index.sqlite")).unwrap(),
        ));
        Fixture {
            _tmp: tmp,
            index,
            config,
        }
    }

    fn recorder(f: &Fixture, secs: usize, finalize: Arc<Finalize>) -> Recorder {
        Recorder::new(f.config.clone(), f.index.clone(), Arc::default())
            .with_fakes(fake_start(secs), finalize)
    }

    fn at(minute: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 27, 14, minute, 0).unwrap()
    }

    fn auto_start() -> Action {
        Action::StartRecording {
            source_app: "brave".into(),
            by_hotkey: false,
        }
    }

    fn jobs(f: &Fixture) -> usize {
        let index = lock(&f.index);
        let mut n = 0;
        while index.next_job().unwrap().is_some() {
            n += 1;
        }
        n
    }

    #[test]
    fn slow_finalize_does_not_delay_the_next_recording() {
        let f = fixture(1);
        let (release, released) = mpsc::channel::<()>();
        let released = Mutex::new(released);
        let mut rec = recorder(
            &f,
            2,
            Arc::new(move |cache: &Path, out: &Path| {
                let _ = lock(&released).recv();
                finalize(cache, out)
            }),
        );
        rec.apply(Action::StartCache, at(0)).unwrap();
        rec.apply(auto_start(), at(0)).unwrap();

        let t = Instant::now();
        rec.apply(Action::Finish, at(30)).unwrap();
        rec.apply(Action::StartCache, at(31)).unwrap();
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert!(rec.is_capturing());
        assert_eq!(rec.keeps_pending(), 1);

        release.send(()).unwrap();
        rec.wait_keeps();
        assert_eq!(rec.keeps_pending(), 0);
        assert_eq!(jobs(&f), 1);
    }

    #[test]
    fn failed_keep_is_retried_after_the_delay() {
        let f = fixture(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let mut rec = recorder(
            &f,
            2,
            Arc::new(move |cache: &Path, out: &Path| {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    anyhow::bail!("No space left on device");
                }
                finalize(cache, out)
            }),
        );
        rec.apply(Action::StartCache, at(0)).unwrap();
        rec.apply(auto_start(), at(0)).unwrap();
        rec.apply(Action::Finish, at(30)).unwrap();
        rec.wait_keeps();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(rec.keeps_pending(), 1, "waiting to retry");
        assert_eq!(jobs(&f), 0);

        rec.poll_keeps(Instant::now() + KEEP_RETRY_DELAY / 2);
        rec.wait_keeps();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "not due yet");

        rec.poll_keeps(Instant::now() + KEEP_RETRY_DELAY);
        rec.wait_keeps();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(rec.keeps_pending(), 0);
        assert_eq!(jobs(&f), 1);
    }

    #[test]
    fn hotkey_recordings_skip_the_minimum_length() {
        let f = fixture(120);
        let mut rec = recorder(&f, 2, Arc::new(|c: &Path, o: &Path| finalize(c, o)));
        // Detected: 2 s is under the minimum, so it is deleted.
        rec.apply(Action::StartCache, at(0)).unwrap();
        rec.apply(auto_start(), at(0)).unwrap();
        rec.apply(Action::Finish, at(1)).unwrap();
        rec.wait_keeps();
        assert_eq!(jobs(&f), 0);
        assert_eq!(fs::read_dir(&f.config.meetings_dir).unwrap().count(), 0);

        // Started by hand: kept.
        rec.apply(
            Action::StartRecording {
                source_app: "hotkey".into(),
                by_hotkey: true,
            },
            at(2),
        )
        .unwrap();
        rec.apply(Action::Finish, at(3)).unwrap();
        rec.wait_keeps();
        assert_eq!(jobs(&f), 1);
    }
}
