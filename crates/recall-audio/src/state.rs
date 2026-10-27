//! The recording session state machine.
//!
//! [`Session`] is a pure function of the events fed to it: it never reads a
//! clock. Time advances only through [`DetectEvent::Tick`], and events that
//! carry no time (mic and hotkey events) happen at the last tick seen.

use std::time::{Duration, Instant};

/// A PipeWire client holding a microphone stream open.
#[derive(Clone, Debug, PartialEq)]
pub struct MicUser {
    pub node_id: u32,
    pub binary: String,
    pub app_name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DetectEvent {
    MicOpened(MicUser),
    /// The stream with this node id closed.
    MicClosed(u32),
    /// The detector's PipeWire connection ended; it sends nothing more. The
    /// session ignores this; the daemon restarts the detector.
    DetectorDied(String),
    HotkeyStart,
    HotkeyStop,
    HotkeyDiscard,
    /// The capture that `StartCache` or `StartRecording` asked for did not
    /// start. The session drops back to idle; allowlisted apps still holding
    /// the mic arm again [`CAPTURE_RETRY_DELAY`] later.
    CaptureFailed,
    Tick(Instant),
}

/// How long after a failed capture start the session arms again while an
/// allowlisted app still holds the mic.
pub const CAPTURE_RETRY_DELAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    /// An allowlisted app opened the mic; audio is cached until the debounce
    /// elapses, so a short probe of the mic does not become a recording.
    Armed {
        since: Instant,
    },
    /// `since` is when capture began, including the armed period. `users` are
    /// the allowlisted streams still open. `closed_at` is when the last of them
    /// closed, taken from the last tick; the recording finishes once it is
    /// `grace` old. A hotkey-started recording also finishes this way when an
    /// allowlisted app it tracks closes the mic and the grace passes.
    Recording {
        since: Instant,
        users: Vec<MicUser>,
        closed_at: Option<Instant>,
    },
}

/// What the caller must do after an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Start capturing into a cache that may still be discarded.
    StartCache,
    /// Keep the cached audio and continue it as a recording. `by_hotkey`
    /// is set when the user started it by hand rather than the debounce.
    StartRecording {
        source_app: String,
        by_hotkey: bool,
    },
    /// Stop and keep the recording.
    Finish,
    /// Stop and drop the cache or recording.
    Discard,
    None,
}

/// `source_app` of a recording started by hotkey with no allowlisted app open.
const HOTKEY_SOURCE: &str = "hotkey";

#[derive(Debug)]
pub struct Session {
    phase: Phase,
    allowlist: Vec<String>,
    debounce: Duration,
    grace: Duration,
    /// Allowlisted streams open while armed, or seen before the first tick.
    pending: Vec<MicUser>,
    /// The last tick seen, `None` until the first tick. A `HotkeyStart` before
    /// then is ignored; mic users seen before then arm on the first tick.
    now: Option<Instant>,
    /// After [`DetectEvent::CaptureFailed`]: the earliest tick that arms
    /// again for the users still in `pending`.
    retry_at: Option<Instant>,
}

impl Session {
    pub fn new(allowlist: Vec<String>, debounce: Duration, grace: Duration) -> Self {
        Self {
            phase: Phase::Idle,
            allowlist: allowlist.iter().map(|a| a.to_lowercase()).collect(),
            debounce,
            grace,
            pending: Vec::new(),
            now: None,
            retry_at: None,
        }
    }

    pub fn handle(&mut self, ev: DetectEvent) -> Action {
        match ev {
            DetectEvent::Tick(t) => self.tick(t),
            DetectEvent::MicOpened(user) => self.mic_opened(user),
            DetectEvent::MicClosed(node_id) => self.mic_closed(node_id),
            DetectEvent::DetectorDied(_) => Action::None,
            DetectEvent::HotkeyStart => self.hotkey_start(),
            DetectEvent::HotkeyStop => match self.phase {
                Phase::Recording { .. } => self.reset(Action::Finish),
                // Nothing was kept yet, so stopping drops the cache.
                Phase::Armed { .. } => self.reset(Action::Discard),
                Phase::Idle => Action::None,
            },
            DetectEvent::HotkeyDiscard => match self.phase {
                Phase::Idle => Action::None,
                _ => self.reset(Action::Discard),
            },
            DetectEvent::CaptureFailed => self.capture_failed(),
        }
    }

    /// The binary of the first allowlisted app waiting to be recorded.
    pub fn pending_app(&self) -> Option<&str> {
        self.pending.first().map(|u| u.binary.as_str())
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    fn tick(&mut self, t: Instant) -> Action {
        self.now = Some(t);
        match self.phase {
            Phase::Idle
                if !self.pending.is_empty() && self.retry_at.is_none_or(|retry| t >= retry) =>
            {
                self.retry_at = None;
                self.phase = Phase::Armed { since: t };
                Action::StartCache
            }
            Phase::Armed { since } if t.duration_since(since) >= self.debounce => {
                self.start_recording(since, false)
            }
            Phase::Recording {
                closed_at: Some(closed),
                ..
            } if t.duration_since(closed) >= self.grace => self.reset(Action::Finish),
            _ => Action::None,
        }
    }

    fn mic_opened(&mut self, user: MicUser) -> Action {
        if !self.allowed(&user) {
            return Action::None;
        }
        match &mut self.phase {
            Phase::Idle => {
                let Some(now) = self.now else {
                    // No time yet to arm at: the first tick arms instead, so
                    // a meeting already running at startup is not lost.
                    self.pending.push(user);
                    return Action::None;
                };
                // A new app opening the mic arms at once, even while a failed
                // start waits to retry.
                self.pending.push(user);
                self.retry_at = None;
                self.phase = Phase::Armed { since: now };
                Action::StartCache
            }
            Phase::Armed { .. } => {
                self.pending.push(user);
                Action::None
            }
            Phase::Recording {
                users, closed_at, ..
            } => {
                users.push(user);
                *closed_at = None;
                Action::None
            }
        }
    }

    fn mic_closed(&mut self, node_id: u32) -> Action {
        match &mut self.phase {
            Phase::Idle => {
                remove_node(&mut self.pending, node_id);
                Action::None
            }
            Phase::Armed { .. } => {
                if !remove_node(&mut self.pending, node_id) || !self.pending.is_empty() {
                    return Action::None;
                }
                self.reset(Action::Discard)
            }
            Phase::Recording {
                users, closed_at, ..
            } => {
                if remove_node(users, node_id) && users.is_empty() {
                    *closed_at = self.now;
                }
                Action::None
            }
        }
    }

    fn hotkey_start(&mut self) -> Action {
        match self.phase {
            Phase::Idle => match self.now {
                Some(now) => self.start_recording(now, true),
                None => Action::None,
            },
            Phase::Armed { since } => self.start_recording(since, true),
            // Asking to record during the grace period keeps the recording.
            Phase::Recording {
                ref mut closed_at, ..
            } => {
                *closed_at = None;
                Action::None
            }
        }
    }

    /// Moves the pending users into a recording that began at `since`.
    fn start_recording(&mut self, since: Instant, by_hotkey: bool) -> Action {
        let users = std::mem::take(&mut self.pending);
        let source_app = users
            .first()
            .map_or_else(|| HOTKEY_SOURCE.to_owned(), |u| u.binary.clone());
        self.phase = Phase::Recording {
            since,
            users,
            closed_at: None,
        };
        Action::StartRecording {
            source_app,
            by_hotkey,
        }
    }

    /// Back to idle after a capture failed to start, keeping the allowlisted
    /// users that still hold the mic so the next tick after the retry delay
    /// arms again. A user closing the mic meanwhile leaves `pending` as
    /// usual, so nothing retries once they are all gone.
    fn capture_failed(&mut self) -> Action {
        if let Phase::Recording { users, .. } = std::mem::replace(&mut self.phase, Phase::Idle) {
            self.pending.extend(users);
        }
        self.retry_at = self.now.map(|now| now + CAPTURE_RETRY_DELAY);
        Action::None
    }

    fn reset(&mut self, action: Action) -> Action {
        self.phase = Phase::Idle;
        self.pending.clear();
        self.retry_at = None;
        action
    }

    fn allowed(&self, user: &MicUser) -> bool {
        self.allowlist.iter().any(|a| {
            user.binary.eq_ignore_ascii_case(a) || user.app_name.to_lowercase().contains(a)
        })
    }
}

/// Removes the user with `node_id`, returning whether one was tracked.
fn remove_node(users: &mut Vec<MicUser>, node_id: u32) -> bool {
    let before = users.len();
    users.retain(|u| u.node_id != node_id);
    users.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBOUNCE: Duration = Duration::from_secs(8);
    const GRACE: Duration = Duration::from_secs(30);

    fn session() -> Session {
        Session::new(vec!["brave".into(), "zoom".into()], DEBOUNCE, GRACE)
    }

    fn user(node_id: u32, binary: &str, app_name: &str) -> MicUser {
        MicUser {
            node_id,
            binary: binary.into(),
            app_name: app_name.into(),
        }
    }

    fn brave() -> MicUser {
        user(1, "brave", "Brave Browser")
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// A session already recording `brave()`, armed at `t0`.
    fn recording(t0: Instant) -> Session {
        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        s.handle(DetectEvent::MicOpened(brave()));
        s.handle(DetectEvent::Tick(t0 + secs(9)));
        assert!(matches!(s.phase(), Phase::Recording { .. }));
        s
    }

    #[test]
    fn allowlisted_mic_arms_then_records_after_debounce() {
        let t0 = Instant::now();
        let mut s = session();
        assert_eq!(s.handle(DetectEvent::Tick(t0)), Action::None);
        assert_eq!(
            s.handle(DetectEvent::MicOpened(brave())),
            Action::StartCache
        );
        assert_eq!(s.phase(), &Phase::Armed { since: t0 });
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(5))), Action::None);
        assert_eq!(
            s.handle(DetectEvent::Tick(t0 + secs(9))),
            Action::StartRecording {
                source_app: "brave".into(),
                by_hotkey: false,
            }
        );
        assert_eq!(
            s.phase(),
            &Phase::Recording {
                since: t0,
                users: vec![brave()],
                closed_at: None,
            }
        );
    }

    #[test]
    fn probe_closed_within_debounce_discards() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        s.handle(DetectEvent::MicOpened(brave()));
        s.handle(DetectEvent::Tick(t0 + secs(3)));
        assert_eq!(s.handle(DetectEvent::MicClosed(1)), Action::Discard);
        assert_eq!(s.phase(), &Phase::Idle);
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(20))), Action::None);
    }

    #[test]
    fn non_allowlisted_app_is_ignored() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        let other = user(2, "obs", "OBS Studio");
        assert_eq!(s.handle(DetectEvent::MicOpened(other)), Action::None);
        assert_eq!(s.phase(), &Phase::Idle);
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(60))), Action::None);
        assert_eq!(s.handle(DetectEvent::MicClosed(2)), Action::None);
    }

    #[test]
    fn matching_uses_binary_or_app_name() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        let zoom = user(3, "ZoomWebinar", "Zoom Meeting");
        assert_eq!(s.handle(DetectEvent::MicOpened(zoom)), Action::StartCache);

        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        let upper = user(4, "BRAVE", "whatever");
        assert_eq!(s.handle(DetectEvent::MicOpened(upper)), Action::StartCache);

        // Allowlist entries match case-insensitively against app_name too.
        let mut s = Session::new(vec!["Zoom".into()], DEBOUNCE, GRACE);
        s.handle(DetectEvent::Tick(t0));
        let zoom_us = user(5, "CptHost", "zoom.us");
        assert_eq!(
            s.handle(DetectEvent::MicOpened(zoom_us)),
            Action::StartCache
        );
    }

    #[test]
    fn second_app_joins_current_recording() {
        let t0 = Instant::now();
        let mut s = recording(t0);
        let zoom = user(2, "zoom", "Zoom");
        assert_eq!(s.handle(DetectEvent::MicOpened(zoom.clone())), Action::None);
        assert_eq!(
            s.phase(),
            &Phase::Recording {
                since: t0,
                users: vec![brave(), zoom],
                closed_at: None,
            }
        );
        // Only the last tracked user closing starts the grace period.
        s.handle(DetectEvent::Tick(t0 + secs(20)));
        assert_eq!(s.handle(DetectEvent::MicClosed(1)), Action::None);
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(100))), Action::None);
        assert_eq!(s.handle(DetectEvent::MicClosed(2)), Action::None);
        assert!(matches!(
            s.phase(),
            Phase::Recording { closed_at: Some(c), .. } if *c == t0 + secs(100)
        ));
    }

    #[test]
    fn close_then_grace_finishes() {
        let t0 = Instant::now();
        let mut s = recording(t0);
        let closed = t0 + secs(60);
        s.handle(DetectEvent::Tick(closed));
        assert_eq!(s.handle(DetectEvent::MicClosed(1)), Action::None);
        assert_eq!(s.handle(DetectEvent::Tick(closed + secs(10))), Action::None);
        assert_eq!(
            s.handle(DetectEvent::Tick(closed + secs(31))),
            Action::Finish
        );
        assert_eq!(s.phase(), &Phase::Idle);
    }

    #[test]
    fn reopen_within_grace_cancels_finish() {
        let t0 = Instant::now();
        let mut s = recording(t0);
        let closed = t0 + secs(60);
        s.handle(DetectEvent::Tick(closed));
        s.handle(DetectEvent::MicClosed(1));
        s.handle(DetectEvent::Tick(closed + secs(10)));
        let reopened = user(5, "brave", "Brave Browser");
        assert_eq!(s.handle(DetectEvent::MicOpened(reopened)), Action::None);
        assert_eq!(s.handle(DetectEvent::Tick(closed + secs(31))), Action::None);
        assert!(matches!(
            s.phase(),
            Phase::Recording {
                closed_at: None,
                ..
            }
        ));
    }

    #[test]
    fn detector_died_changes_nothing() {
        let t0 = Instant::now();
        let mut s = recording(t0);
        let before = s.phase().clone();
        assert_eq!(
            s.handle(DetectEvent::DetectorDied("gone".into())),
            Action::None
        );
        assert_eq!(*s.phase(), before);
    }

    #[test]
    fn unknown_node_close_is_ignored() {
        let t0 = Instant::now();
        let mut s = recording(t0);
        assert_eq!(s.handle(DetectEvent::MicClosed(99)), Action::None);
        assert!(matches!(
            s.phase(),
            Phase::Recording {
                closed_at: None,
                ..
            }
        ));
    }

    #[test]
    fn hotkey_start_from_idle_records_immediately() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        assert_eq!(
            s.handle(DetectEvent::HotkeyStart),
            Action::StartRecording {
                source_app: "hotkey".into(),
                by_hotkey: true,
            }
        );
        assert_eq!(
            s.phase(),
            &Phase::Recording {
                since: t0,
                users: vec![],
                closed_at: None,
            }
        );
        assert_eq!(s.handle(DetectEvent::HotkeyStart), Action::None);
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(600))), Action::None);
        assert_eq!(s.handle(DetectEvent::HotkeyStop), Action::Finish);
        assert_eq!(s.phase(), &Phase::Idle);
    }

    #[test]
    fn hotkey_start_while_armed_uses_pending_app() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        s.handle(DetectEvent::MicOpened(brave()));
        s.handle(DetectEvent::Tick(t0 + secs(2)));
        assert_eq!(
            s.handle(DetectEvent::HotkeyStart),
            Action::StartRecording {
                source_app: "brave".into(),
                by_hotkey: true,
            }
        );
        assert_eq!(
            s.phase(),
            &Phase::Recording {
                since: t0,
                users: vec![brave()],
                closed_at: None,
            }
        );
    }

    #[test]
    fn hotkey_stop_finishes_without_grace() {
        let t0 = Instant::now();
        let mut s = recording(t0);
        assert_eq!(s.handle(DetectEvent::HotkeyStop), Action::Finish);
        assert_eq!(s.phase(), &Phase::Idle);
    }

    #[test]
    fn hotkey_discard_while_recording() {
        let t0 = Instant::now();
        let mut s = recording(t0);
        assert_eq!(s.handle(DetectEvent::HotkeyDiscard), Action::Discard);
        assert_eq!(s.phase(), &Phase::Idle);
    }

    #[test]
    fn hotkeys_while_idle_do_nothing() {
        let mut s = session();
        s.handle(DetectEvent::Tick(Instant::now()));
        assert_eq!(s.handle(DetectEvent::HotkeyStop), Action::None);
        assert_eq!(s.handle(DetectEvent::HotkeyDiscard), Action::None);
        assert_eq!(s.phase(), &Phase::Idle);
    }

    #[test]
    fn mic_opened_before_first_tick_arms_on_that_tick() {
        let t0 = Instant::now();
        let mut s = session();
        assert_eq!(s.handle(DetectEvent::MicOpened(brave())), Action::None);
        let zoom = user(2, "zoom", "Zoom");
        assert_eq!(s.handle(DetectEvent::MicOpened(zoom)), Action::None);
        assert_eq!(s.handle(DetectEvent::MicClosed(2)), Action::None);
        assert_eq!(s.phase(), &Phase::Idle);
        assert_eq!(s.handle(DetectEvent::Tick(t0)), Action::StartCache);
        assert_eq!(s.phase(), &Phase::Armed { since: t0 });
        assert_eq!(
            s.handle(DetectEvent::Tick(t0 + secs(9))),
            Action::StartRecording {
                source_app: "brave".into(),
                by_hotkey: false,
            }
        );
        assert_eq!(
            s.phase(),
            &Phase::Recording {
                since: t0,
                users: vec![brave()],
                closed_at: None,
            }
        );
    }

    #[test]
    fn hotkey_start_during_grace_cancels_finish() {
        let t0 = Instant::now();
        let mut s = recording(t0);
        let closed = t0 + secs(60);
        s.handle(DetectEvent::Tick(closed));
        s.handle(DetectEvent::MicClosed(1));
        assert_eq!(s.handle(DetectEvent::HotkeyStart), Action::None);
        assert!(matches!(
            s.phase(),
            Phase::Recording {
                closed_at: None,
                ..
            }
        ));
        assert_eq!(s.handle(DetectEvent::Tick(closed + secs(31))), Action::None);
        assert_eq!(s.handle(DetectEvent::HotkeyStop), Action::Finish);
    }

    #[test]
    fn failed_capture_start_retries_while_the_app_holds_the_mic() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        assert_eq!(
            s.handle(DetectEvent::MicOpened(brave())),
            Action::StartCache
        );
        assert_eq!(s.handle(DetectEvent::CaptureFailed), Action::None);
        assert_eq!(s.phase(), &Phase::Idle);
        assert_eq!(s.pending_app(), Some("brave"));
        // Nothing until the retry delay has passed.
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(1))), Action::None);
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(9))), Action::None);
        assert_eq!(
            s.handle(DetectEvent::Tick(t0 + CAPTURE_RETRY_DELAY)),
            Action::StartCache
        );
        assert_eq!(
            s.phase(),
            &Phase::Armed {
                since: t0 + CAPTURE_RETRY_DELAY
            }
        );

        // A second failure retries again; the app closing the mic stops it.
        s.handle(DetectEvent::CaptureFailed);
        s.handle(DetectEvent::Tick(t0 + secs(15)));
        assert_eq!(s.handle(DetectEvent::MicClosed(1)), Action::None);
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(60))), Action::None);
        assert_eq!(s.phase(), &Phase::Idle);
    }

    #[test]
    fn failed_hotkey_start_does_not_retry() {
        let t0 = Instant::now();
        let mut s = session();
        s.handle(DetectEvent::Tick(t0));
        s.handle(DetectEvent::HotkeyStart);
        assert_eq!(s.handle(DetectEvent::CaptureFailed), Action::None);
        assert_eq!(s.phase(), &Phase::Idle);
        assert_eq!(s.handle(DetectEvent::Tick(t0 + secs(60))), Action::None);
    }
}
