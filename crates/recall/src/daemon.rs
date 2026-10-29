//! The long-running daemon: the recorder thread, the control socket, the
//! processing worker and the HTTP server.
//!
//! The recorder runs on a plain thread, not a tokio task: it owns the
//! detector and the capture, whose PipeWire objects stay on their own
//! threads. Finished recordings are encoded on threads of their own (see
//! [`Recorder::poll_keeps`]), which shutdown waits for. Everything else
//! talks to the recorder by sending [`DetectEvent`]s into its channel and
//! reads its [`StatusSnapshot`] and the live recording it publishes in a
//! [`LiveSlot`].
//!
//! The worker is another plain thread (see [`crate::worker`]). It is never
//! joined: stopping the daemon does not wait for a transcription that can
//! take minutes. The step it leaves running is marked failed by
//! [`sweep_interrupted`] on the next start, and its job runs again.
//!
//! With `vault.remote` set, the vault sync is a third plain thread (see
//! [`crate::vault`]), fed by the HTTP handlers and by a task that forwards
//! the end of every job. It is not joined either.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local};
use recall_audio::detect::Detector;
use recall_audio::state::{Action, DetectEvent, Phase, Session};
use recall_server::{AppState, ControlCommand, StatusSnapshot};
use recall_store::{Config, Event, Index, MeetingId, Speakers, Step};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

use crate::control::{self, RecorderLink};
use crate::meet_sink::{LiveSlot, MeetSink, Seen};
use crate::notify;
use crate::recorder::Recorder;
use crate::selftest;
use crate::vault::VaultSync;
use crate::worker::{Worker, load_real_engines, sweep_interrupted};

const TICK: Duration = Duration::from_secs(1);
/// `application.name` of recall's own capture streams, which the detector
/// must not report as a meeting app.
const OWN_APP_NAME: &str = "recall";
const DETECTOR_RETRY_DELAY: Duration = Duration::from_secs(2);
/// More detector deaths (failed restarts included) than this within
/// [`DETECTOR_DEATH_WINDOW`] end the daemon with an error, leaving systemd to
/// restart the unit.
const MAX_DETECTOR_DEATHS: usize = 5;
const DETECTOR_DEATH_WINDOW: Duration = Duration::from_secs(60);
const LOCK_FILE: &str = "daemon.lock";
/// Events buffered for each slow web client before it starts missing some.
const EVENT_BUFFER: usize = 256;

/// Runs the daemon until SIGINT or SIGTERM, or until the recorder or the
/// control socket fails.
pub async fn run(config: Config) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init();

    // Held until `run` returns. Without it a second daemon's recovery would
    // take this one's live cache for a crashed recording, and its `serve`
    // would replace this one's socket.
    let _lock = lock_daemon()?;
    let socket = control::socket_path();

    let config = Arc::new(config);
    let index = Index::open(&config.index_path())
        .with_context(|| format!("opening {}", config.index_path().display()))?;
    let speakers = Speakers::load(&config.speakers_path())?;
    let swept = sweep_interrupted(&config, &index, &speakers)?;
    if !swept.is_empty() {
        tracing::info!(count = swept.len(), "marked interrupted runs failed");
    }
    index.reset_running_jobs()?;
    // Folders deleted, renamed or copied in by hand since the last run.
    match index.rebuild(&config.meetings_dir, &speakers) {
        Ok(report) => {
            for (id, err) in &report.failed {
                tracing::warn!(id = id.0, err, "left out of the index");
            }
        }
        Err(err) => tracing::warn!(err = format!("{err:#}"), "rebuilding the index"),
    }
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
        meet: None,
        screen: None,
    }));
    let live = Arc::new(LiveSlot::default());
    let (events, _) = broadcast::channel(EVENT_BUFFER);
    let (wake, woken) = mpsc::channel();
    // Detached; see the module docs.
    Worker {
        config: config.clone(),
        index: index.clone(),
        speakers: speakers.clone(),
        events: events.clone(),
        load: load_real_engines(),
        notifications: true,
    }
    .spawn(woken)?;

    let (tx, rx) = mpsc::channel();
    let link = RecorderLink {
        events: tx.clone(),
        screen_off: Arc::new(AtomicBool::new(false)),
    };
    let shutdown = Arc::new(AtomicBool::new(false));
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let recorder_thread = {
        let recorder =
            Recorder::new(config.clone(), index.clone(), speakers.clone()).with_wake(wake.clone());
        let config = config.clone();
        let tx = tx.clone();
        let status = status.clone();
        let live = live.clone();
        let screen_off = link.screen_off.clone();
        let shutdown = shutdown.clone();
        let events = events.clone();
        std::thread::Builder::new()
            .name("recall-recorder".into())
            .spawn(move || {
                let reporter = StatusReporter::new(&status, &live);
                let result = recorder_loop(
                    &config,
                    recorder,
                    Inbox {
                        rx: &rx,
                        tx: &tx,
                        screen_off: &screen_off,
                    },
                    reporter,
                    &events,
                    &shutdown,
                );
                let _ = done_tx.send(result);
            })
            .context("spawning the recorder thread")?
    };

    let vault = start_vault(&config, &speakers, &index, &events);
    let state = AppState {
        config: config.clone(),
        config_path: Config::path(),
        index: index.clone(),
        speakers,
        events,
        llm: recall_llm::from_config_or_unavailable(&config.llm),
        status: status.clone(),
        control: {
            let link = link.clone();
            Arc::new(move |cmd: ControlCommand| link.send(control::Command::from(cmd)))
        },
        selftest: Arc::new(selftest::run),
        rerun: {
            let index = index.clone();
            Arc::new(move |id: &MeetingId, step: Step| {
                let job = index
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .enqueue(id, step)?;
                let _ = wake.send(());
                Ok(job)
            })
        },
        meet: {
            let status = status.clone();
            let seen = Arc::new(Mutex::new(Seen::default()));
            Arc::new(move |batch| {
                MeetSink {
                    live: &live,
                    status: &status,
                    seen: &seen,
                }
                .accept(batch)
            })
        },
        vault,
    };
    let mut http_server = tokio::spawn(recall_server::serve(state));

    let mut control_server = {
        let socket = socket.clone();
        let status = status.clone();
        tokio::spawn(async move { control::serve(&socket, link, status).await })
    };

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
        result = &mut http_server => match result {
            Ok(Err(err)) => Err(err.context("the HTTP server failed")),
            _ => Err(anyhow!("the HTTP server stopped")),
        },
    };

    control_server.abort();
    http_server.abort();
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
    remove_file(&socket);
    remove_file(&notify::state_path());
    outcome
}

/// Starts the vault sync's thread and the task that feeds it finished jobs,
/// when `vault.remote` is set. Like the worker it is never joined: a push
/// cut short by the exit is redone from the ledger on the next start. A
/// sync that cannot start is logged and leaves publishing off rather than
/// stopping the daemon.
fn start_vault(
    config: &Arc<Config>,
    speakers: &Arc<Mutex<Speakers>>,
    index: &Arc<Mutex<Index>>,
    events: &broadcast::Sender<Event>,
) -> Option<Arc<dyn recall_server::Vault>> {
    if !config.vault.enabled() {
        return None;
    }
    let started = VaultSync::new(config.clone(), speakers.clone(), index.clone())
        .and_then(|(vault, syncer)| syncer.spawn().map(|_| vault));
    match started {
        Ok(vault) => {
            tokio::spawn(vault.clone().follow(events.subscribe()));
            Some(vault)
        }
        Err(err) => {
            tracing::error!(err = format!("{err:#}"), "starting the vault sync");
            None
        }
    }
}

/// Takes an exclusive lock on `<runtime dir>/daemon.lock`, failing if another
/// daemon holds it. The lock lasts as long as the returned file is open, and
/// the kernel drops it when the process dies, so a crash leaves nothing
/// stale.
fn lock_daemon() -> Result<std::fs::File> {
    let dir = control::runtime_dir();
    control::create_private_dir(&dir)?;
    let path = dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(rustix::io::Errno::WOULDBLOCK) => {
            bail!(
                "another recall daemon is running (it holds {})",
                path.display()
            )
        }
        Err(err) => Err(err).with_context(|| format!("locking {}", path.display())),
    }
}

/// What reaches the recorder thread: detector and control events on `rx`
/// (`tx` is its sender, for restarted detectors), and the screen-off
/// request.
struct Inbox<'a> {
    rx: &'a Receiver<DetectEvent>,
    tx: &'a Sender<DetectEvent>,
    screen_off: &'a AtomicBool,
}

/// Feeds detector, control and tick events to the session and carries out
/// its actions until `shutdown` is set, and turns the screen recording off
/// within a tick of a request. On shutdown a recording in progress is kept
/// and a cache still being armed is dropped. When the detector dies too
/// often, the loop stops the same way and then returns an error.
fn recorder_loop(
    config: &Config,
    mut recorder: Recorder,
    inbox: Inbox,
    mut reporter: StatusReporter,
    events: &broadcast::Sender<Event>,
    shutdown: &AtomicBool,
) -> Result<()> {
    let mut session = Session::new(
        config.allowlist.clone(),
        Duration::from_secs(config.debounce_secs),
        Duration::from_secs(config.end_grace_secs),
    );
    let Inbox { rx, tx, screen_off } = inbox;
    let mut deaths = DeathWindow::default();
    let mut detector = match Detector::spawn(tx.clone(), OWN_APP_NAME) {
        Ok(detector) => Some(detector),
        Err(err) => {
            tracing::warn!(err = format!("{err:#}"), "starting the detector");
            restart_detector(tx, shutdown, &mut deaths)?
        }
    };
    let mut next_tick = Instant::now();
    let mut failure = None;

    while !shutdown.load(Ordering::Acquire) {
        match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
            Ok(DetectEvent::DetectorDied(reason)) => {
                tracing::warn!(reason, "the detector died; restarting it");
                drop(detector.take());
                match restart_detector(tx, shutdown, &mut deaths) {
                    Ok(restarted) => detector = restarted,
                    Err(err) => {
                        failure = Some(err);
                        break;
                    }
                }
            }
            Ok(event) => step(&mut session, &mut recorder, &mut reporter, events, event),
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
                events,
                DetectEvent::Tick(now),
            );
            recorder.poll_keeps(now);
            recorder.poll_screen();
            if recorder.capture_failed() {
                tracing::error!("the capture stopped on its own; ending the recording");
                notify::failed(recorder.source_app().unwrap_or("recall"), "recording audio");
                step(
                    &mut session,
                    &mut recorder,
                    &mut reporter,
                    events,
                    DetectEvent::HotkeyStop,
                );
            }
        }
        if screen_off.swap(false, Ordering::AcqRel) {
            recorder.screen_off();
        }
        reporter.update(&session, &recorder);
    }

    drop(detector);
    match session.phase() {
        Phase::Recording { .. } => {
            reporter.finishing();
            apply(&mut recorder, events, Action::Finish);
        }
        Phase::Armed { .. } => {
            reporter.end_live();
            apply(&mut recorder, events, Action::Discard);
        }
        Phase::Idle => {}
    }
    recorder.wait_keeps();
    failure.map_or(Ok(()), Err)
}

/// Hands `event` to the session and carries out the resulting action. A
/// capture that fails to start is notified, and the session retries it
/// while an allowlisted app still holds the mic.
fn step(
    session: &mut Session,
    recorder: &mut Recorder,
    reporter: &mut StatusReporter,
    events: &broadcast::Sender<Event>,
    event: DetectEvent,
) {
    let action = session.handle(event);
    let app = match &action {
        Action::StartRecording { source_app, .. } => Some(source_app.clone()),
        Action::StartCache => Some(session.pending_app().unwrap_or("the microphone").to_owned()),
        _ => None,
    };
    match action {
        Action::Finish => reporter.finishing(),
        Action::Discard => reporter.end_live(),
        _ => {}
    }
    if !apply(recorder, events, action)
        && let Some(app) = app
    {
        // No capture is running, so drop back to idle rather than show a
        // recording that is not happening. One notification per run of
        // failures: the session retries every few seconds.
        if !reporter.start_failed {
            notify::failed(&app, "starting the recording");
        }
        reporter.start_failed = true;
        session.handle(DetectEvent::CaptureFailed);
    } else if recorder.is_capturing() {
        reporter.start_failed = false;
    }
}

/// Applies `action`, logging a failure, and broadcasts
/// [`Event::Recording`] for a recording that started and [`Event::Stopped`]
/// for one that ended (failed or not). Returns whether it succeeded.
fn apply(recorder: &mut Recorder, events: &broadcast::Sender<Event>, action: Action) -> bool {
    let name = format!("{action:?}");
    let event = match &action {
        Action::StartRecording { source_app, .. } => Some(Event::Recording {
            app: source_app.clone(),
        }),
        Action::Finish | Action::Discard => Some(Event::Stopped),
        Action::None | Action::StartCache => None,
    };
    let applied = recorder.apply(action, Local::now());
    if let Some(event) = event
        && (applied.is_ok() || matches!(event, Event::Stopped))
    {
        let _ = events.send(event);
    }
    match applied {
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

/// Spawns a new detector [`DETECTOR_RETRY_DELAY`] after each death, a
/// failed spawn counting as another death. Errors once [`DeathWindow`] says
/// it has died too often; returns `None` if the daemon is stopping.
fn restart_detector(
    tx: &Sender<DetectEvent>,
    shutdown: &AtomicBool,
    deaths: &mut DeathWindow,
) -> Result<Option<Detector>> {
    loop {
        if !deaths.record(Instant::now()) {
            bail!(
                "the detector died more than {MAX_DETECTOR_DEATHS} times within {} s",
                DETECTOR_DEATH_WINDOW.as_secs()
            );
        }
        std::thread::sleep(DETECTOR_RETRY_DELAY);
        if shutdown.load(Ordering::Acquire) {
            return Ok(None);
        }
        match Detector::spawn(tx.clone(), OWN_APP_NAME) {
            Ok(detector) => {
                tracing::info!("the detector restarted");
                return Ok(Some(detector));
            }
            Err(err) => tracing::warn!(err = format!("{err:#}"), "restarting the detector"),
        }
    }
}

/// Recent detector deaths, for bounding restarts.
#[derive(Debug, Default)]
struct DeathWindow {
    deaths: VecDeque<Instant>,
}

impl DeathWindow {
    /// Records a death at `at`. Returns false when more than
    /// [`MAX_DETECTOR_DEATHS`] deaths fall within the
    /// [`DETECTOR_DEATH_WINDOW`] ending at `at`.
    fn record(&mut self, at: Instant) -> bool {
        while self
            .deaths
            .front()
            .is_some_and(|&d| at.duration_since(d) >= DETECTOR_DEATH_WINDOW)
        {
            self.deaths.pop_front();
        }
        self.deaths.push_back(at);
        self.deaths.len() <= MAX_DETECTOR_DEATHS
    }
}

/// Publishes the recorder's state to the shared snapshot and `state.json`,
/// and the recording in progress to the [`LiveSlot`] the Meet sink reads.
struct StatusReporter<'a> {
    status: &'a RwLock<StatusSnapshot>,
    live: &'a LiveSlot,
    state_path: std::path::PathBuf,
    last: StatusSnapshot,
    /// Set once a failed capture start has been notified, until a capture
    /// starts again.
    start_failed: bool,
}

impl<'a> StatusReporter<'a> {
    fn new(status: &'a RwLock<StatusSnapshot>, live: &'a LiveSlot) -> Self {
        let last = status
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let reporter = Self {
            status,
            live,
            state_path: notify::state_path(),
            last,
            start_failed: false,
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
        let current = recorder.live();
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if *live != current {
            *live = current;
        }
        drop(live);
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
            meet: None,
            screen: recorder.screen_status().map(str::to_owned),
        });
    }

    /// Empties the [`LiveSlot`] before a recording is finished or
    /// discarded. It waits for a Meet batch being written, and no later one
    /// writes to this recording's directory.
    fn end_live(&self) {
        *self.live.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Shown while a finished recording is encoded and saved.
    fn finishing(&mut self) {
        self.end_live();
        let snapshot = StatusSnapshot {
            phase: "finishing".into(),
            since_ms: unix_ms(Local::now()),
            mic_peak: 0.0,
            sys_peak: 0.0,
            ..self.last.clone()
        };
        self.publish(snapshot);
    }

    fn publish(&mut self, mut snapshot: StatusSnapshot) {
        let mut shared = self.status.write().unwrap_or_else(PoisonError::into_inner);
        carry_shared(&mut snapshot, &shared);
        // The shared snapshot can differ from `last` when the Meet sink set
        // `meet` outside a recording; storing it again clears that.
        if *shared != snapshot {
            *shared = snapshot.clone();
        }
        drop(shared);
        if snapshot != self.last {
            self.last = snapshot;
            self.write_file();
        }
    }

    fn write_file(&self) {
        if let Err(err) = notify::write_state(&self.state_path, &self.last) {
            tracing::warn!(err = format!("{err:#}"), "writing the state file");
        }
    }
}

/// Copies into the recorder's `snapshot` the field the Meet sink writes to
/// the shared one: `meet`, kept only while recording, so it clears when a
/// recording ends. The caller holds the write lock across this and the
/// store, so neither writer loses the other's update.
fn carry_shared(snapshot: &mut StatusSnapshot, shared: &StatusSnapshot) {
    snapshot.meet = if snapshot.phase == "recording" {
        shared.meet.clone()
    } else {
        None
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meet_status_is_kept_only_while_recording() {
        let shared = StatusSnapshot {
            phase: "recording".into(),
            since_ms: 1,
            source_app: Some("brave".into()),
            mic_peak: 0.0,
            sys_peak: 0.0,
            meet: Some("signal_lost".into()),
            screen: Some("recording".into()),
        };
        let mut next = StatusSnapshot {
            mic_peak: 0.5,
            meet: None,
            screen: Some("error".into()),
            ..shared.clone()
        };
        carry_shared(&mut next, &shared);
        assert_eq!(next.meet.as_deref(), Some("signal_lost"));
        // The recorder owns `screen`.
        assert_eq!(next.screen.as_deref(), Some("error"));

        for phase in ["finishing", "idle", "armed"] {
            let mut next = StatusSnapshot {
                phase: phase.into(),
                ..next.clone()
            };
            carry_shared(&mut next, &shared);
            assert_eq!(next.meet, None, "{phase}");
        }
    }

    #[test]
    fn publish_clears_meet_set_outside_a_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let armed = StatusSnapshot {
            phase: "armed".into(),
            since_ms: 1,
            source_app: Some("brave".into()),
            mic_peak: 0.0,
            sys_peak: 0.0,
            meet: None,
            screen: None,
        };
        let status = RwLock::new(armed.clone());
        let live = LiveSlot::default();
        let mut reporter = StatusReporter {
            status: &status,
            live: &live,
            state_path: tmp.path().join("state.json"),
            last: armed.clone(),
            start_failed: false,
        };
        // The sink stores a status while armed, or after a finished
        // recording's last publish; the next unchanged snapshot clears it.
        status.write().unwrap().meet = Some("signal_lost".into());
        reporter.publish(armed.clone());
        assert_eq!(status.read().unwrap().meet, None);

        let idle = StatusSnapshot {
            phase: "idle".into(),
            ..armed
        };
        reporter.publish(idle.clone());
        status.write().unwrap().meet = Some("ok".into());
        reporter.publish(idle);
        assert_eq!(status.read().unwrap().meet, None);
    }

    #[test]
    fn death_window_allows_five_deaths_a_minute() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let mut window = DeathWindow::default();
        for i in 0..5 {
            assert!(window.record(t0 + secs(i * 10)), "death {i}");
        }
        // The sixth within 60 s of the first is one too many.
        assert!(!window.record(t0 + secs(59)));

        let mut window = DeathWindow::default();
        for i in 0..5 {
            assert!(window.record(t0 + secs(i * 10)));
        }
        // At 60 s the first death has left the window.
        assert!(window.record(t0 + secs(60)));
        // Deaths spaced over a minute apart never trip it.
        let mut window = DeathWindow::default();
        assert!((0..100).all(|i| window.record(t0 + secs(i * 61))));
        assert!(window.deaths.len() == 1);
    }
}
