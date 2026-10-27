//! The long-running daemon: the recorder thread, the control socket, and
//! (in later tasks) the processing worker and the HTTP server.
//!
//! The recorder runs on a plain thread, not a tokio task: it owns the
//! detector and the capture, whose PipeWire objects stay on their own
//! threads, and it blocks while encoding a finished recording. Everything
//! else talks to it by sending [`DetectEvent`]s into its channel and reads
//! its [`StatusSnapshot`].

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local};
use recall_audio::detect::Detector;
use recall_audio::state::{Action, DetectEvent, Phase, Session};
use recall_server::StatusSnapshot;
use recall_store::{Config, Index, Speakers};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::control;
use crate::notify;
use crate::recorder::Recorder;

const TICK: Duration = Duration::from_secs(1);
/// `application.name` of recall's own capture streams, which the detector
/// must not report as a meeting app.
const OWN_APP_NAME: &str = "recall";
const DETECTOR_RETRY_DELAY: Duration = Duration::from_secs(2);
const DETECTOR_RETRIES: u32 = 5;

/// Runs the daemon until SIGINT or SIGTERM, or until the recorder or the
/// control socket fails.
pub async fn run(config: Config) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    // Recovery would take a running daemon's live cache for a crashed one,
    // and `serve` would replace its socket.
    let socket = control::socket_path();
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        bail!("another recall daemon is listening on {}", socket.display());
    }

    let config = Arc::new(config);
    let index = Index::open(&config.index_path())
        .with_context(|| format!("opening {}", config.index_path().display()))?;
    let speakers = Speakers::load(&config.speakers_path())?;
    index.reset_running_jobs()?;
    let recovered = Recorder::recover(&config, &index, &speakers)?;
    if !recovered.is_empty() {
        tracing::info!(count = recovered.len(), "queued recovered recordings");
    }
    let index = Arc::new(Mutex::new(index));
    let speakers = Arc::new(Mutex::new(speakers));

    let status = Arc::new(RwLock::new(StatusSnapshot {
        phase: "idle".into(),
        since_ms: unix_ms(Local::now()),
        source_app: None,
        mic_peak: 0.0,
        sys_peak: 0.0,
    }));
    let (tx, rx) = mpsc::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let recorder_thread = {
        let recorder = Recorder::new(config.clone(), index.clone(), speakers.clone());
        let config = config.clone();
        let tx = tx.clone();
        let status = status.clone();
        let shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("recall-recorder".into())
            .spawn(move || {
                let result = recorder_loop(&config, recorder, &rx, &tx, &status, &shutdown);
                let _ = done_tx.send(result);
            })
            .context("spawning the recorder thread")?
    };

    let mut control_server = {
        let socket = socket.clone();
        let status = status.clone();
        tokio::spawn(async move { control::serve(&socket, tx, status).await })
    };

    // Task 12: worker

    // Task 17: HTTP server

    let mut sigterm = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    let outcome = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("SIGINT; stopping");
            Ok(())
        }
        _ = sigterm.recv() => {
            tracing::info!("SIGTERM; stopping");
            Ok(())
        }
        result = &mut done_rx => match result {
            Ok(Ok(())) => Err(anyhow!("the recorder stopped")),
            Ok(Err(err)) => Err(err.context("the recorder failed")),
            Err(_) => Err(anyhow!("the recorder thread panicked")),
        },
        result = &mut control_server => match result {
            Ok(Err(err)) => Err(err.context("the control socket failed")),
            _ => Err(anyhow!("the control socket stopped")),
        },
    };

    control_server.abort();
    remove_file(&socket);
    shutdown.store(true, Ordering::Release);
    // The recorder sees the flag within a tick, then finishes any recording,
    // which can take a while for a long one.
    let joined = tokio::task::spawn_blocking(move || recorder_thread.join()).await;
    if !matches!(joined, Ok(Ok(()))) {
        tracing::error!("the recorder thread panicked");
    }
    if let Ok(Err(err)) = done_rx.try_recv() {
        tracing::error!(
            err = format!("{err:#}"),
            "the recorder failed while stopping"
        );
    }
    remove_file(&notify::state_path());
    outcome
}

/// Feeds detector, control and tick events to the session and carries out
/// its actions until `shutdown` is set. On shutdown a recording in progress
/// is kept and a cache still being armed is dropped. Returns an error only
/// when the detector cannot be restarted.
fn recorder_loop(
    config: &Config,
    mut recorder: Recorder,
    rx: &Receiver<DetectEvent>,
    tx: &Sender<DetectEvent>,
    status: &RwLock<StatusSnapshot>,
    shutdown: &AtomicBool,
) -> Result<()> {
    let mut session = Session::new(
        config.allowlist.clone(),
        Duration::from_secs(config.debounce_secs),
        Duration::from_secs(config.end_grace_secs),
    );
    let mut detector = Some(match Detector::spawn(tx.clone(), OWN_APP_NAME) {
        Ok(detector) => detector,
        Err(err) => {
            tracing::warn!(err = format!("{err:#}"), "starting the detector");
            restart_detector(tx, shutdown)?
        }
    });
    let mut reporter = StatusReporter::new(status);
    let mut next_tick = Instant::now();

    while !shutdown.load(Ordering::Acquire) {
        match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Ok(DetectEvent::DetectorDied(reason)) => {
                tracing::warn!(reason, "the detector died; restarting it");
                drop(detector.take());
                detector = Some(restart_detector(tx, shutdown)?);
            }
            Ok(event) => step(&mut session, &mut recorder, &mut reporter, event),
            Err(RecvTimeoutError::Timeout) => {}
            // `tx` is ours, so the channel never disconnects.
            Err(RecvTimeoutError::Disconnected) => unreachable!("the recorder holds a sender"),
        }
        let now = Instant::now();
        if now >= next_tick {
            next_tick = now + TICK;
            step(
                &mut session,
                &mut recorder,
                &mut reporter,
                DetectEvent::Tick(now),
            );
            if recorder.capture_failed() {
                tracing::error!("the capture stopped on its own; ending the recording");
                step(
                    &mut session,
                    &mut recorder,
                    &mut reporter,
                    DetectEvent::HotkeyStop,
                );
            }
        }
        reporter.update(&session, &recorder);
    }

    drop(detector);
    match session.phase() {
        Phase::Recording { .. } => {
            reporter.finishing();
            apply(&mut recorder, Action::Finish);
        }
        Phase::Armed { .. } => {
            apply(&mut recorder, Action::Discard);
        }
        Phase::Idle => {}
    }
    Ok(())
}

/// Hands `event` to the session and carries out the resulting action.
fn step(
    session: &mut Session,
    recorder: &mut Recorder,
    reporter: &mut StatusReporter,
    event: DetectEvent,
) {
    let action = session.handle(event);
    let starting = matches!(action, Action::StartCache | Action::StartRecording { .. });
    if action == Action::Finish {
        reporter.finishing();
    }
    if !apply(recorder, action) && starting {
        // No capture is running, so drop back to idle rather than show a
        // recording that is not happening. The app holding the mic arms a
        // new attempt only when it opens the mic again.
        let action = session.handle(DetectEvent::HotkeyDiscard);
        apply(recorder, action);
    }
}

/// Applies `action`, logging a failure. Returns whether it succeeded.
fn apply(recorder: &mut Recorder, action: Action) -> bool {
    let name = format!("{action:?}");
    match recorder.apply(action, Local::now()) {
        Ok(()) => true,
        Err(err) => {
            tracing::error!(
                err = format!("{err:#}"),
                action = name,
                "recorder action failed"
            );
            false
        }
    }
}

/// Spawns a new detector, trying up to [`DETECTOR_RETRIES`] times
/// [`DETECTOR_RETRY_DELAY`] apart.
fn restart_detector(tx: &Sender<DetectEvent>, shutdown: &AtomicBool) -> Result<Detector> {
    for attempt in 1..=DETECTOR_RETRIES {
        std::thread::sleep(DETECTOR_RETRY_DELAY);
        if shutdown.load(Ordering::Acquire) {
            bail!("stopping while restarting the detector");
        }
        match Detector::spawn(tx.clone(), OWN_APP_NAME) {
            Ok(detector) => {
                tracing::info!(attempt, "the detector restarted");
                return Ok(detector);
            }
            Err(err) => {
                tracing::warn!(err = format!("{err:#}"), attempt, "restarting the detector")
            }
        }
    }
    bail!("the detector did not restart after {DETECTOR_RETRIES} attempts")
}

/// Publishes the recorder's state to the shared snapshot and `state.json`.
struct StatusReporter<'a> {
    status: &'a RwLock<StatusSnapshot>,
    state_path: std::path::PathBuf,
    last: StatusSnapshot,
}

impl<'a> StatusReporter<'a> {
    fn new(status: &'a RwLock<StatusSnapshot>) -> Self {
        let last = status
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let reporter = Self {
            status,
            state_path: notify::state_path(),
            last,
        };
        reporter.write_file();
        reporter
    }

    fn update(&mut self, session: &Session, recorder: &Recorder) {
        let phase = match session.phase() {
            Phase::Idle => "idle",
            Phase::Armed { .. } => "armed",
            Phase::Recording { .. } => "recording",
        };
        let (mic_peak, sys_peak) = recorder.peaks();
        let since_ms = match (phase, recorder.started()) {
            // A recording counts from when its capture started, armed time
            // included, as its meeting directory does.
            ("recording", Some(started)) => unix_ms(started),
            _ if phase == self.last.phase => self.last.since_ms,
            _ => unix_ms(Local::now()),
        };
        self.publish(StatusSnapshot {
            phase: phase.into(),
            since_ms,
            source_app: recorder.source_app().map(str::to_owned),
            mic_peak,
            sys_peak,
        });
    }

    /// Shown while a finished recording is encoded and saved.
    fn finishing(&mut self) {
        let snapshot = StatusSnapshot {
            phase: "finishing".into(),
            since_ms: unix_ms(Local::now()),
            mic_peak: 0.0,
            sys_peak: 0.0,
            ..self.last.clone()
        };
        self.publish(snapshot);
    }

    fn publish(&mut self, snapshot: StatusSnapshot) {
        if snapshot == self.last {
            return;
        }
        *self.status.write().unwrap_or_else(PoisonError::into_inner) = snapshot.clone();
        self.last = snapshot;
        self.write_file();
    }

    fn write_file(&self) {
        if let Err(err) = notify::write_state(&self.state_path, &self.last) {
            tracing::warn!(err = format!("{err:#}"), "writing the state file");
        }
    }
}

fn unix_ms(t: DateTime<Local>) -> u64 {
    u64::try_from(t.timestamp_millis()).unwrap_or(0)
}

fn remove_file(path: &Path) {
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%err, path = %path.display(), "removing a runtime file");
    }
}
