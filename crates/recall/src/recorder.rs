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
//!
//! With `record_screen` on, a recording also runs `gpu-screen-recorder`
//! from `StartRecording` on (see [`crate::screen`]). Keeping and recovery
//! remux its `.cache/screen.mkv` into `screen.mp4` before the cache is
//! cleared; an orphaned video counts as unfinished work for recovery.

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
use recall_store::{
    CaptureClock, Config, Index, Meeting, MeetingDir, MeetingId, Speakers, Step, StepState,
};

use crate::notify;
use crate::screen::{self, ScreenRecorder, ScreenTools};

/// `"YYYY-MM-DD HH:MM"`, the start of every meeting directory name.
const ID_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";
const ID_TIME_LEN: usize = 16;
/// How long after a failed keep (a full disk, say) it is tried again.
pub const KEEP_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Free space in `meetings_dir` below which starting a recording warns.
const LOW_SPACE_BYTES: u64 = 2_000_000_000;
/// Where a screen video that could not be remuxed is kept.
const SCREEN_KEPT: &str = "screen.mkv";

/// What the recorder needs from a running capture: [`Capture`] in the
/// daemon, a fake in tests.
pub trait CaptureControl: Send {
    /// False once the capture has ended on its own.
    fn is_running(&self) -> bool;
    /// Stops it and returns the first error it hit.
    fn stop(self: Box<Self>) -> Result<()>;
    /// The wall-clock and monotonic time of the capture's origin.
    fn clock(&self) -> CaptureClock;
}

impl CaptureControl for Capture {
    fn is_running(&self) -> bool {
        Capture::is_running(self)
    }

    fn stop(self: Box<Self>) -> Result<()> {
        Capture::stop(*self)
    }

    fn clock(&self) -> CaptureClock {
        Capture::clock(self)
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
    /// The capture's origin, also in `.cache/clock.json`.
    clock: CaptureClock,
    source_app: Option<String>,
    /// Why the screen recording failed to start or stopped early.
    screen_error: Option<String>,
    /// The screen recording was turned off for this meeting.
    screen_off: bool,
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
    /// The screen recorder of `active`, while it runs.
    screen: Option<ScreenRecorder>,
    screen_tools: Arc<ScreenTools>,
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
            screen: None,
            screen_tools: Arc::new(ScreenTools::from_env()),
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

    /// Replaces the screen recording's programs and token; for tests.
    pub fn with_screen(mut self, tools: ScreenTools) -> Self {
        self.screen_tools = Arc::new(tools);
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
    /// - `StartRecording` remembers the source app, notifies and, with
    ///   `record_screen` on, starts the screen recorder. A hotkey start from
    ///   idle comes without `StartCache`, so it starts the capture first.
    /// - `Finish` stops the capture and starts keeping the recording on a
    ///   thread of its own (see [`Recorder::poll_keeps`]), whatever its
    ///   length; only a recording with no audio at all is deleted.
    /// - `Discard` stops the capture and the screen recorder and deletes the
    ///   directory.
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
                if let Some(active) = &mut self.active {
                    if self.notifications {
                        notify::recording_started(
                            &active.dir.id.0,
                            &active.title,
                            &source_app,
                            self.config.port,
                        );
                    }
                    active.source_app = Some(source_app);
                }
                self.start_screen();
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
        let ffmpeg = self.screen_tools.ffmpeg.clone();
        let wake = self.wake.clone();
        let notifications = self.notifications;
        let job = active.clone();
        let run = move || {
            let kept = keep(
                &job.dir,
                &job.title,
                job.started.to_utc(),
                job.source_app.clone(),
                job.screen_error.clone(),
                finalize.as_ref(),
                &ffmpeg,
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
                        notify::recording_saved(
                            &meeting.id.0,
                            &meeting.title,
                            meeting.duration_ms,
                            config.port,
                        );
                    }
                    if let Some(wake) = &wake {
                        let _ = wake.send(());
                    }
                }
                None => tracing::info!(id = job.dir.id.0, "recording has no audio; deleted"),
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

    /// Starts the screen recorder for the active recording when
    /// `record_screen` is on and it has not been started, turned off or
    /// failed. A failure is recorded in the meeting and notified; the audio
    /// goes on.
    fn start_screen(&mut self) {
        let Some(active) = &mut self.active else {
            return;
        };
        if !self.config.record_screen
            || self.screen.is_some()
            || active.screen_off
            || active.screen_error.is_some()
        {
            return;
        }
        let tools = &self.screen_tools;
        match ScreenRecorder::start(&tools.gsr, &active.dir.screen_cache(), &tools.token) {
            Ok(recorder) => {
                tracing::info!(id = active.dir.id.0, "recording the screen");
                self.screen = Some(recorder);
            }
            Err(err) => {
                let err = format!("{err:#}");
                tracing::error!(err, id = active.dir.id.0, "starting the screen recorder");
                if self.notifications {
                    notify::screen_failed(&err);
                }
                active.screen_error = Some(err);
            }
        }
    }

    /// Notices a screen recorder that exited on its own: the meeting records
    /// it and a notification says so. The audio goes on, and the video
    /// written until then is kept.
    pub fn poll_screen(&mut self) {
        let Some(recorder) = &mut self.screen else {
            return;
        };
        if recorder.is_running() {
            return;
        }
        let err = match recorder.exit_status() {
            Some(status) => format!("the screen recorder exited early ({status})"),
            None => "the screen recorder exited early".to_owned(),
        };
        self.screen = None;
        tracing::error!(err, "screen recording stopped");
        if self.notifications {
            notify::screen_failed(&err);
        }
        if let Some(active) = &mut self.active {
            active.screen_error = Some(err);
        }
    }

    /// Turns the screen recording off for the active recording: stops the
    /// recorder and deletes the video. Does nothing without a recording.
    pub fn screen_off(&mut self) {
        let Some(active) = &mut self.active else {
            return;
        };
        if active.screen_off {
            return;
        }
        active.screen_off = true;
        let discarded = match self.screen.take() {
            Some(recorder) => recorder.discard(),
            // It may have exited early and left a partial video.
            None => screen::remove_files(&active.dir.screen_cache()),
        };
        match discarded {
            Ok(()) => tracing::info!(id = active.dir.id.0, "screen recording turned off"),
            Err(err) => tracing::warn!(
                err = format!("{err:#}"),
                id = active.dir.id.0,
                "discarding the screen recording"
            ),
        }
    }

    /// The screen recording's state for the status chip. While recording:
    /// `"off"` when not recorded or turned off, `"error"` when it failed,
    /// else `"recording"`. Otherwise, with `record_screen` on, `"picker"`
    /// when no portal token is saved and the next recording will show the
    /// picker.
    pub fn screen_status(&self) -> Option<&'static str> {
        match &self.active {
            // `source_app` is set by `StartRecording`, where the screen
            // recorder starts; before it the cache is only armed.
            Some(active) if active.source_app.is_some() => Some(if active.screen_off {
                "off"
            } else if active.screen_error.is_some() {
                "error"
            } else if self.screen.is_some() {
                "recording"
            } else {
                "off"
            }),
            _ if self.config.record_screen
                && screen::token_status(&self.screen_tools.token) == "picker" =>
            {
                Some("picker")
            }
            _ => None,
        }
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

    /// The directory and origin clock of the capture in progress.
    pub fn live(&self) -> Option<(MeetingDir, CaptureClock)> {
        let active = self.active.as_ref()?;
        Some((active.dir.clone(), active.clock))
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
        // Without the file the audio is still kept; only screen and Meet
        // alignment for this recording is lost.
        let clock = capture.clock();
        if let Err(err) = clock.write(&dir.clock()) {
            tracing::warn!(err = format!("{err:#}"), "writing the capture clock");
        }
        self.capture = Some(capture);
        self.active = Some(Active {
            dir,
            title,
            started: now,
            clock,
            source_app: None,
            screen_error: None,
            screen_off: false,
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

    /// Stops the capture and the screen recorder and returns what they were
    /// recording. Errors are logged, not returned: the chunks and video
    /// already on disk are still usable. Stopping the screen recorder waits
    /// for it to finish its file, up to 10 s.
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
        if let Some(recorder) = self.screen.take()
            && let Err(err) = recorder.stop()
        {
            tracing::error!(
                err = format!("{err:#}"),
                id = active.dir.id.0,
                "stopping the screen recorder"
            );
        }
        Some(active)
    }

    /// Finishes every recording under `config.meetings_dir` whose `.cache/`
    /// still holds chunks or a screen video, as a crash or kill leaves them:
    /// an empty cache is deleted with its directory, the rest is encoded, its video remuxed, and saved, indexed and queued
    /// from [`Step::Segment`]. Returns the ids queued. A directory that
    /// fails is logged and skipped.
    pub fn recover(config: &Config, index: &Index, speakers: &Speakers) -> Result<Vec<MeetingId>> {
        Self::recover_with(config, index, speakers, &ScreenTools::from_env())
    }

    /// [`Recorder::recover`] with the given screen tools.
    pub fn recover_with(
        config: &Config,
        index: &Index,
        speakers: &Speakers,
        screen: &ScreenTools,
    ) -> Result<Vec<MeetingId>> {
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
            match recover_dir(index, speakers, &screen.ffmpeg, &dir) {
                Ok(Some(id)) => {
                    tracing::info!(id = id.0, "recovered a recording");
                    queued.push(id);
                }
                Ok(None) => tracing::info!(id = dir.id.0, "deleted an empty unfinished recording"),
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
    index: &Index,
    speakers: &Speakers,
    ffmpeg: &Path,
    dir: &MeetingDir,
) -> Result<Option<MeetingId>> {
    let path = dir.path();
    if path.join("meeting.json").is_file() {
        // Saved, so the audio was complete: the crash came after `save`.
        let mut meeting = Meeting::load(&path)?;
        if dir.screen_cache().exists() {
            let (offset, error) = finish_screen(dir, ffmpeg, meeting.screen_error.take());
            meeting.screen_offset_ms = offset;
            meeting.screen_error = error;
            meeting.save(&path)?;
        }
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
    let kept = keep(
        dir,
        title,
        started,
        None,
        None,
        &finalize,
        ffmpeg,
        |meeting| queue(index, speakers, meeting),
    )?;
    Ok(kept.map(|meeting| meeting.id))
}

/// Encodes the cache of `dir` with `finalize`, remuxes its screen video
/// with `ffmpeg` (see [`finish_screen`]), saves `meeting.json`, hands the
/// meeting to `queue` (index and enqueue) and clears the cache; or deletes
/// `dir` if the cache is empty. Returns the meeting kept.
#[allow(clippy::too_many_arguments)]
fn keep(
    dir: &MeetingDir,
    title: &str,
    started: DateTime<Utc>,
    source_app: Option<String>,
    screen_error: Option<String>,
    finalize: &Finalize,
    ffmpeg: &Path,
    queue: impl FnOnce(&Meeting) -> Result<()>,
) -> Result<Option<Meeting>> {
    let cache = dir.cache();
    let cached = cache_duration(&cache)?;
    if cached.is_zero() {
        remove_dir(&dir.path())?;
        return Ok(None);
    }
    let duration = finalize(&cache, &dir.audio())?;
    let (screen_offset_ms, screen_error) = finish_screen(dir, ffmpeg, screen_error);
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
        screen_offset_ms,
        screen_error,
        sync: false,
    };
    meeting.save(&dir.path())?;
    queue(&meeting)?;
    dir.clear_cache()?;
    Ok(Some(meeting))
}

/// Remuxes `dir`'s `.cache/screen.mkv`, if there is one, into
/// `screen.mp4` and returns the video's offset from `.cache/clock.json`
/// with `error`, the screen error so far. A failed remux moves the MKV to
/// `screen.mkv` beside the audio, so `clear_cache` does not delete the
/// video, and adds the failure to the error.
fn finish_screen(
    dir: &MeetingDir,
    ffmpeg: &Path,
    error: Option<String>,
) -> (Option<i64>, Option<String>) {
    let mkv = dir.screen_cache();
    if !mkv.exists() {
        return (None, error);
    }
    let clock = CaptureClock::read(&dir.clock()).unwrap_or_else(|err| {
        tracing::warn!(err = format!("{err:#}"), "reading the capture clock");
        None
    });
    let err = match screen::finish(ffmpeg, &mkv, &dir.screen(), clock.as_ref()) {
        Ok(offset) => return (offset, error),
        Err(err) => err,
    };
    let kept = dir.path().join(SCREEN_KEPT);
    let mut message = format!("remuxing the screen recording: {err:#}");
    match fs::rename(&mkv, &kept) {
        Ok(()) => message.push_str(&format!("; the video is kept as {SCREEN_KEPT}")),
        Err(rename) => {
            tracing::error!(%rename, "keeping the screen recording");
        }
    }
    tracing::error!(
        err = message,
        id = dir.id.0,
        "finishing the screen recording"
    );
    let error = match error {
        Some(earlier) => format!("{earlier}; {message}"),
        None => message,
    };
    (None, Some(error))
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

        fn clock(&self) -> CaptureClock {
            FAKE_CLOCK
        }
    }

    const FAKE_CLOCK: CaptureClock = CaptureClock {
        wall_ms: 1_793_170_000_000,
        monotonic_us: 5_000_000,
    };

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

    fn fixture() -> Fixture {
        fixture_with_screen(false)
    }

    fn fixture_with_screen(record_screen: bool) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let config = Arc::new(Config {
            meetings_dir: tmp.path().join("meetings"),
            data_dir: tmp.path().join("data"),
            record_screen,
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
            .with_screen(screen::fake::tools(f._tmp.path()))
    }

    fn screen_recorder(f: &Fixture, tools: ScreenTools) -> Recorder {
        recorder(f, 2, Arc::new(|c: &Path, o: &Path| finalize(c, o))).with_screen(tools)
    }

    /// Starts a hotkey recording and waits for the fake screen recorder to
    /// write its file. Returns the meeting directory.
    fn record_with_screen(rec: &mut Recorder) -> MeetingDir {
        rec.apply(hotkey_start(), at(0)).unwrap();
        let (dir, _) = rec.live().unwrap();
        screen::fake::wait_for(&dir.screen_cache());
        dir
    }

    fn hotkey_start() -> Action {
        Action::StartRecording {
            source_app: "hotkey".into(),
            by_hotkey: true,
        }
    }

    /// Finishes the recording and returns its saved meeting.
    fn finish_and_load(rec: &mut Recorder, dir: &MeetingDir) -> Meeting {
        rec.apply(Action::Finish, at(1)).unwrap();
        rec.wait_keeps();
        assert_eq!(rec.keeps_pending(), 0);
        Meeting::load(&dir.path()).unwrap()
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
        let f = fixture();
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
        let f = fixture();
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
    fn short_detected_recordings_are_kept() {
        let f = fixture();
        let mut rec = recorder(&f, 2, Arc::new(|c: &Path, o: &Path| finalize(c, o)));
        rec.apply(Action::StartCache, at(0)).unwrap();
        rec.apply(auto_start(), at(0)).unwrap();
        rec.apply(Action::Finish, at(1)).unwrap();
        rec.wait_keeps();
        assert_eq!(jobs(&f), 1);
    }

    #[test]
    fn recordings_without_audio_are_deleted() {
        let f = fixture();
        let mut rec = recorder(&f, 0, Arc::new(|c: &Path, o: &Path| finalize(c, o)));
        rec.apply(Action::StartCache, at(0)).unwrap();
        rec.apply(auto_start(), at(0)).unwrap();
        rec.apply(Action::Finish, at(1)).unwrap();
        rec.wait_keeps();
        assert_eq!(jobs(&f), 0);
        assert_eq!(fs::read_dir(&f.config.meetings_dir).unwrap().count(), 0);
    }

    #[test]
    fn start_writes_clock_file() {
        let f = fixture();
        let mut rec = recorder(&f, 2, Arc::new(|c: &Path, o: &Path| finalize(c, o)));
        assert!(rec.live().is_none());
        rec.apply(Action::StartCache, at(0)).unwrap();

        let (dir, clock) = rec.live().unwrap();
        assert_eq!(clock, FAKE_CLOCK);
        assert_eq!(CaptureClock::read(&dir.clock()).unwrap(), Some(FAKE_CLOCK));

        rec.apply(Action::Finish, at(1)).unwrap();
        assert!(rec.live().is_none());
        rec.wait_keeps();
    }
    #[test]
    fn screen_is_remuxed_with_its_offset() {
        let f = fixture_with_screen(true);
        let mut rec = screen_recorder(&f, screen::fake::tools(f._tmp.path()));
        let dir = record_with_screen(&mut rec);
        assert_eq!(rec.screen_status(), Some("recording"));

        let meeting = finish_and_load(&mut rec, &dir);
        // The fake's first frame is at 5,250,000 µs, the capture's origin
        // at 5,000,000 µs.
        assert_eq!(meeting.screen_offset_ms, Some(250));
        assert_eq!(meeting.screen_error, None);
        assert_eq!(fs::read(dir.screen()).unwrap(), b"video");
        assert!(dir.audio().is_file());
        assert!(!dir.cache().exists());
    }

    #[test]
    fn screen_is_not_recorded_when_turned_off_in_config() {
        let f = fixture_with_screen(false);
        let mut rec = screen_recorder(&f, screen::fake::tools(f._tmp.path()));
        rec.apply(hotkey_start(), at(0)).unwrap();
        let (dir, _) = rec.live().unwrap();
        assert_eq!(rec.screen_status(), Some("off"));
        let meeting = finish_and_load(&mut rec, &dir);
        assert!(!dir.screen().exists());
        assert_eq!(meeting.screen_error, None);
    }

    #[test]
    fn start_failure_keeps_audio_and_records_error() {
        let f = fixture_with_screen(true);
        let tools = ScreenTools {
            gsr: f._tmp.path().join("no-such-recorder"),
            ..screen::fake::tools(f._tmp.path())
        };
        let mut rec = screen_recorder(&f, tools);
        rec.apply(hotkey_start(), at(0)).unwrap();
        assert!(rec.is_capturing());
        assert_eq!(rec.screen_status(), Some("error"));
        let (dir, _) = rec.live().unwrap();

        let meeting = finish_and_load(&mut rec, &dir);
        assert!(dir.audio().is_file());
        assert!(!dir.screen().exists());
        assert_eq!(meeting.screen_offset_ms, None);
        let err = meeting.screen_error.unwrap();
        assert!(err.contains("no-such-recorder"), "{err}");
    }

    #[test]
    fn early_exit_records_error() {
        let f = fixture_with_screen(true);
        let tools = ScreenTools {
            gsr: screen::fake::fakes().exiting_gsr.clone(),
            ..screen::fake::tools(f._tmp.path())
        };
        let mut rec = screen_recorder(&f, tools);
        rec.apply(hotkey_start(), at(0)).unwrap();
        let (dir, _) = rec.live().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rec.screen_status() != Some("error") {
            assert!(Instant::now() < deadline, "the exit was never seen");
            rec.poll_screen();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(rec.is_capturing());

        let meeting = finish_and_load(&mut rec, &dir);
        assert!(dir.audio().is_file());
        assert!(meeting.screen_error.unwrap().contains("exited"));
    }

    #[test]
    fn screen_off_deletes_video() {
        let f = fixture_with_screen(true);
        let mut rec = screen_recorder(&f, screen::fake::tools(f._tmp.path()));
        let dir = record_with_screen(&mut rec);

        rec.screen_off();
        assert!(!dir.screen_cache().exists());
        assert!(!screen::ts_path(&dir.screen_cache()).exists());
        assert_eq!(rec.screen_status(), Some("off"));
        // A second request with nothing recording does nothing.
        rec.screen_off();

        let meeting = finish_and_load(&mut rec, &dir);
        assert!(!dir.screen().exists());
        assert_eq!(meeting.screen_offset_ms, None);
        assert_eq!(meeting.screen_error, None);
        assert!(dir.audio().is_file());
    }

    #[test]
    fn remux_failure_keeps_the_mkv() {
        let f = fixture_with_screen(true);
        let tools = ScreenTools {
            ffmpeg: screen::fake::fakes().failing_ffmpeg.clone(),
            ..screen::fake::tools(f._tmp.path())
        };
        let mut rec = screen_recorder(&f, tools);
        let dir = record_with_screen(&mut rec);

        let meeting = finish_and_load(&mut rec, &dir);
        assert!(!dir.screen().exists());
        assert_eq!(fs::read(dir.path().join("screen.mkv")).unwrap(), b"video");
        assert!(meeting.screen_error.unwrap().contains("Invalid data"));
        assert!(!dir.cache().exists());
    }

    #[test]
    fn screen_status_asks_for_the_picker_when_idle_without_a_token() {
        let f = fixture_with_screen(true);
        let tools = screen::fake::tools(f._tmp.path());
        let token = tools.token.clone();
        let rec = screen_recorder(&f, tools);
        assert_eq!(rec.screen_status(), Some("picker"));
        fs::write(&token, "token").unwrap();
        assert_eq!(rec.screen_status(), None);

        let off = fixture_with_screen(false);
        let rec = screen_recorder(&off, screen::fake::tools(off._tmp.path()));
        assert_eq!(rec.screen_status(), None);
    }

    #[test]
    fn recover_remuxes_before_clearing_cache() {
        let f = fixture_with_screen(true);
        // What a SIGKILL mid-recording leaves: chunks, clock and video.
        let dir = MeetingDir::create(&f.config.meetings_dir, at(0), "Meeting 14:00").unwrap();
        let mut writer = ChunkWriter::new(&dir.cache()).unwrap();
        let silence = vec![0.0f32; 2 * 48_000];
        writer.push(Track::Mic, &silence).unwrap();
        writer.push(Track::Sys, &silence).unwrap();
        writer.flush().unwrap();
        FAKE_CLOCK.write(&dir.clock()).unwrap();
        fs::write(dir.screen_cache(), b"video").unwrap();
        fs::write(
            screen::ts_path(&dir.screen_cache()),
            "monotonic_microsec\trealtime_microsec\n5250000\t1793170000250000\n",
        )
        .unwrap();

        let index = lock(&f.index);
        let tools = screen::fake::tools(f._tmp.path());
        let ids = Recorder::recover_with(&f.config, &index, &Speakers::default(), &tools).unwrap();

        assert_eq!(ids, vec![dir.id.clone()]);
        assert_eq!(fs::read(dir.screen()).unwrap(), b"video");
        assert!(!dir.cache().exists());
        let meeting = Meeting::load(&dir.path()).unwrap();
        assert_eq!(meeting.screen_offset_ms, Some(250));
        assert_eq!(meeting.screen_error, None);
    }

    #[test]
    fn recover_remuxes_an_orphaned_video_of_a_saved_meeting() {
        let f = fixture_with_screen(true);
        let mut rec = screen_recorder(&f, screen::fake::tools(f._tmp.path()));
        rec.apply(Action::StartCache, at(0)).unwrap();
        rec.apply(Action::Finish, at(1)).unwrap();
        rec.wait_keeps();
        let dir = MeetingDir::open(
            &f.config.meetings_dir,
            &MeetingId("2026-10-27 14:00 Meeting 14:00".into()),
        )
        .unwrap();
        // Saved and queued, then killed before the cache was cleared, with
        // only the video left in it.
        FAKE_CLOCK.write(&dir.clock()).unwrap();
        fs::write(dir.screen_cache(), b"video").unwrap();
        assert!(dir.has_unfinished_cache());

        let index = lock(&f.index);
        let tools = screen::fake::tools(f._tmp.path());
        let ids = Recorder::recover_with(&f.config, &index, &Speakers::default(), &tools).unwrap();

        assert_eq!(ids, vec![dir.id.clone()]);
        assert_eq!(fs::read(dir.screen()).unwrap(), b"video");
        assert!(!dir.cache().exists());
        // No `.ts`: the video is kept without an offset.
        assert_eq!(Meeting::load(&dir.path()).unwrap().screen_offset_ms, None);
    }
}
