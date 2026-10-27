//! Turns the session's [`Action`]s into captures, meeting directories and
//! queued jobs, and recovers recordings a crash left in their cache.
//!
//! Keeping a recording runs: encode `audio.opus`, write `meeting.json`,
//! index, enqueue, clear `.cache/`. While `.cache/` holds chunks, recovery
//! redoes the recording: `meeting.json` is written only after the audio is
//! complete, so its presence means only indexing and queueing are left, and
//! both are idempotent ([`Index::enqueue`] returns the existing job).

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

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

/// The recording in progress. Its capture is [`Recorder::capture`].
struct Active {
    dir: MeetingDir,
    title: String,
    started: DateTime<Local>,
    source_app: Option<String>,
}

pub struct Recorder {
    config: Arc<Config>,
    index: Arc<Mutex<Index>>,
    speakers: Arc<Mutex<Speakers>>,
    levels: Arc<Levels>,
    active: Option<Active>,
    /// Set exactly when `active` is.
    capture: Option<Capture>,
    /// Told after a kept recording is queued.
    wake: Option<Sender<()>>,
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
        }
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
    /// - `Finish` stops the capture and keeps the recording, or deletes it if
    ///   it is shorter than `min_length_secs`.
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
            Action::StartRecording { source_app } => {
                if self.active.is_none() {
                    self.start(now)?;
                }
                tracing::info!(source_app, "recording");
                notify::recording_started(&source_app);
                if let Some(active) = &mut self.active {
                    active.source_app = Some(source_app);
                }
                Ok(())
            }
            Action::Finish => {
                let Some(active) = self.stop() else {
                    return Ok(());
                };
                // The encode runs without the locks, which the HTTP server
                // also takes; they are held only to index and queue.
                let kept = keep(
                    &self.config,
                    &active.dir,
                    &active.title,
                    active.started.to_utc(),
                    active.source_app,
                    |meeting| {
                        let speakers = lock(&self.speakers);
                        let index = lock(&self.index);
                        queue(&index, &speakers, meeting)
                    },
                );
                let kept = match kept {
                    Ok(kept) => kept,
                    Err(err) => {
                        notify::failed(&active.title, "saving");
                        return Err(err);
                    }
                };
                match kept {
                    Some(meeting) => {
                        tracing::info!(id = meeting.id.0, "recording kept");
                        notify::processing(&meeting.title);
                        if let Some(wake) = &self.wake {
                            let _ = wake.send(());
                        }
                    }
                    None => tracing::info!(id = active.dir.id.0, "recording too short; deleted"),
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
        self.levels.mic.store(0, Ordering::Relaxed);
        self.levels.sys.store(0, Ordering::Relaxed);
        let capture = ChunkWriter::new(&dir.cache())
            .and_then(|writer| Capture::start(Arc::new(Mutex::new(writer)), self.levels.clone()));
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
        });
        Ok(())
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
    let kept = keep(config, dir, title, started, None, |meeting| {
        queue(index, speakers, meeting)
    })?;
    Ok(kept.map(|meeting| meeting.id))
}

/// Encodes the cache of `dir`, saves `meeting.json`, hands the meeting to
/// `queue` (index and enqueue) and clears the cache; or deletes `dir` if the
/// cache is shorter than `min_length_secs` (or empty). Returns the meeting
/// kept.
fn keep(
    config: &Config,
    dir: &MeetingDir,
    title: &str,
    started: DateTime<Utc>,
    source_app: Option<String>,
    queue: impl FnOnce(&Meeting) -> Result<()>,
) -> Result<Option<Meeting>> {
    let cache = dir.cache();
    let cached = cache_duration(&cache)?;
    if cached.is_zero() || cached < Duration::from_secs(config.min_length_secs) {
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
