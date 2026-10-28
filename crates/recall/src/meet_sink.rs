//! Stores the Meet extension's events on the live recording's timeline.
//!
//! The extension resends the last minute of events with every batch, so the
//! same event arrives many times; it also re-emits names and the roster
//! every 30 s with a fresh `t`, and those are new events.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError, RwLock};

use anyhow::Result;
use recall_server::StatusSnapshot;
use recall_store::{CaptureClock, MeetBatch, MeetEvent, MeetEventKind, MeetingDir, append_events};

/// The recording in progress, as the recorder thread publishes it.
pub type LiveSlot = Mutex<Option<(MeetingDir, CaptureClock)>>;

/// Appends the batch's new events to the live recording's
/// `meet-events.jsonl` and returns how many it appended.
///
/// Nothing is stored without a live recording. Each event's wire `t` (Unix
/// milliseconds) becomes milliseconds from the recording's origin, and
/// events from before the origin are dropped, as are `Unknown` kinds and
/// events already in `seen`, keyed by wire `t` plus the event's JSON. The
/// caller resets `seen` when the live meeting changes.
pub fn accept(
    batch: MeetBatch,
    live: Option<(MeetingDir, CaptureClock)>,
    seen: &mut HashSet<(i64, String)>,
) -> Result<usize> {
    Ok(store(batch, live, seen)?.len())
}

/// [`accept`], returning the stored events.
fn store(
    batch: MeetBatch,
    live: Option<(MeetingDir, CaptureClock)>,
    seen: &mut HashSet<(i64, String)>,
) -> Result<Vec<MeetEvent>> {
    let Some((dir, clock)) = live else {
        return Ok(Vec::new());
    };
    let mut keys = Vec::new();
    let mut events = Vec::new();
    for event in batch.events {
        if event.kind == MeetEventKind::Unknown || event.t < clock.wall_ms {
            continue;
        }
        let key = (event.t, serde_json::to_string(&event.kind)?);
        if seen.contains(&key) || keys.contains(&key) {
            continue;
        }
        keys.push(key);
        events.push(MeetEvent {
            t: event.t - clock.wall_ms,
            kind: event.kind,
        });
    }
    if events.is_empty() {
        return Ok(events);
    }
    append_events(&dir.meet_events(), &events)?;
    // Only once stored, so events lost to a failed write are taken again
    // when they are resent.
    seen.extend(keys);
    Ok(events)
}

/// What the daemon's `POST /api/meet/events` closure calls.
///
/// Lock order: `live`, then `seen`, then `status`.
pub struct MeetSink<'a> {
    pub live: &'a LiveSlot,
    pub status: &'a RwLock<StatusSnapshot>,
    pub seen: &'a Mutex<Seen>,
}

/// The events already stored for one meeting directory.
#[derive(Debug, Default)]
pub struct Seen {
    meeting: Option<PathBuf>,
    keys: HashSet<(i64, String)>,
}

impl MeetSink<'_> {
    /// Stores the batch (see [`accept`]) and sets
    /// [`StatusSnapshot::meet`] from the newest `status` event stored.
    pub fn accept(&self, batch: MeetBatch) -> Result<usize> {
        // Held until the events are written: the recorder clears the slot
        // before it finishes or discards a recording, so no write can land
        // in a directory that is being encoded or removed.
        let slot = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let live = slot.clone();
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let meeting = live.as_ref().map(|(dir, _)| dir.path());
        if seen.meeting != meeting {
            seen.meeting = meeting;
            seen.keys.clear();
        }
        let stored = store(batch, live, &mut seen.keys)?;
        drop(slot);
        let newest = stored
            .iter()
            .filter_map(|e| match &e.kind {
                MeetEventKind::Status { status } => Some((e.t, status)),
                _ => None,
            })
            .max_by_key(|(t, _)| *t);
        if let Some((_, value)) = newest {
            self.status
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .meet = Some(value.clone());
        }
        Ok(stored.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{Local, TimeZone};
    use recall_store::read_events;

    const WALL_MS: i64 = 1_793_170_000_000;

    fn live_dir(root: &std::path::Path, title: &str) -> (MeetingDir, CaptureClock) {
        let started = Local.with_ymd_and_hms(2026, 10, 28, 9, 0, 0).unwrap();
        let dir = MeetingDir::create(root, started, title).unwrap();
        let clock = CaptureClock {
            wall_ms: WALL_MS,
            monotonic_us: 5_000_000,
        };
        (dir, clock)
    }

    fn speaking(t: i64, participant: &str) -> MeetEvent {
        MeetEvent {
            t,
            kind: MeetEventKind::SpeakingStart {
                participant: participant.into(),
            },
        }
    }

    fn signal(t: i64, value: &str) -> MeetEvent {
        MeetEvent {
            t,
            kind: MeetEventKind::Status {
                status: value.into(),
            },
        }
    }

    fn batch(events: Vec<MeetEvent>) -> MeetBatch {
        MeetBatch {
            meeting_code: "abc-defg-hij".into(),
            events,
        }
    }

    fn snapshot() -> StatusSnapshot {
        StatusSnapshot {
            phase: "recording".into(),
            since_ms: 0,
            source_app: None,
            mic_peak: 0.0,
            sys_peak: 0.0,
            meet: None,
            screen: None,
        }
    }

    #[test]
    fn events_before_origin_are_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, clock) = live_dir(tmp.path(), "a");
        let mut seen = HashSet::new();
        let stored = accept(
            batch(vec![
                speaking(WALL_MS - 8_000, "csrc:1"),
                speaking(WALL_MS - 1, "csrc:2"),
                speaking(WALL_MS, "csrc:3"),
                speaking(WALL_MS + 4_500, "csrc:4"),
            ]),
            Some((dir.clone(), clock)),
            &mut seen,
        )
        .unwrap();
        assert_eq!(stored, 2);
        assert_eq!(
            read_events(&dir.meet_events()).unwrap(),
            vec![speaking(0, "csrc:3"), speaking(4_500, "csrc:4")]
        );
    }

    #[test]
    fn resent_events_are_stored_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, clock) = live_dir(tmp.path(), "a");
        let live = Some((dir.clone(), clock));
        let mut seen = HashSet::new();
        let name = |t| MeetEvent {
            t,
            kind: MeetEventKind::Name {
                participant: "csrc:1".into(),
                name: "Tom Lin".into(),
            },
        };
        let first = vec![speaking(WALL_MS + 1_000, "csrc:1"), name(WALL_MS + 1_000)];
        assert_eq!(
            accept(batch(first.clone()), live.clone(), &mut seen).unwrap(),
            2
        );
        // The same events resent, plus a duplicate within the batch and the
        // name re-emitted 30 s later, which is a new event.
        let mut second = first;
        second.push(name(WALL_MS + 31_000));
        second.push(name(WALL_MS + 31_000));
        assert_eq!(
            accept(batch(second.clone()), live.clone(), &mut seen).unwrap(),
            1
        );
        assert_eq!(accept(batch(second), live, &mut seen).unwrap(), 0);
        assert_eq!(
            read_events(&dir.meet_events()).unwrap(),
            vec![speaking(1_000, "csrc:1"), name(1_000), name(31_000)]
        );
    }

    #[test]
    fn unknown_events_are_not_stored() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, clock) = live_dir(tmp.path(), "a");
        let batch: MeetBatch = serde_json::from_value(serde_json::json!({
            "meeting_code": "abc-defg-hij",
            "events": [
                {"t": WALL_MS + 10, "kind": "hand_raised", "participant": "csrc:1"},
                {"t": WALL_MS + 20, "kind": "speaking_start", "participant": "csrc:1"},
            ]
        }))
        .unwrap();
        let mut seen = HashSet::new();
        assert_eq!(
            accept(batch, Some((dir.clone(), clock)), &mut seen).unwrap(),
            1
        );
        let text = std::fs::read_to_string(dir.meet_events()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(!text.contains("unknown"));
    }

    #[test]
    fn nothing_stored_without_live_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let mut seen = HashSet::new();
        let stored = accept(
            batch(vec![speaking(WALL_MS + 1_000, "csrc:1")]),
            None,
            &mut seen,
        )
        .unwrap();
        assert_eq!(stored, 0);
        assert!(seen.is_empty());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);

        let live = LiveSlot::default();
        let status = RwLock::new(StatusSnapshot {
            phase: "idle".into(),
            ..snapshot()
        });
        let seen = Mutex::default();
        let sink = MeetSink {
            live: &live,
            status: &status,
            seen: &seen,
        };
        assert_eq!(
            sink.accept(batch(vec![signal(WALL_MS + 1_000, "signal_lost")]))
                .unwrap(),
            0
        );
        assert_eq!(status.read().unwrap().meet, None);
    }

    #[test]
    fn status_event_updates_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, clock) = live_dir(tmp.path(), "a");
        let live = LiveSlot::new(Some((dir.clone(), clock)));
        let status = RwLock::new(snapshot());
        let seen = Mutex::default();
        let sink = MeetSink {
            live: &live,
            status: &status,
            seen: &seen,
        };

        // A batch without a status event leaves `meet` alone.
        sink.accept(batch(vec![speaking(WALL_MS + 500, "csrc:1")]))
            .unwrap();
        assert_eq!(status.read().unwrap().meet, None);
        // The newest status by `t` wins, whatever the batch order.
        sink.accept(batch(vec![
            signal(WALL_MS + 2_000, "signal_lost"),
            signal(WALL_MS + 1_000, "ok"),
        ]))
        .unwrap();
        assert_eq!(status.read().unwrap().meet.as_deref(), Some("signal_lost"));
        // Resending it changes nothing; a newer one replaces it.
        sink.accept(batch(vec![
            signal(WALL_MS + 1_000, "ok"),
            signal(WALL_MS + 2_000, "signal_lost"),
            signal(WALL_MS + 3_000, "ok"),
        ]))
        .unwrap();
        assert_eq!(status.read().unwrap().meet.as_deref(), Some("ok"));

        // A new recording starts with a fresh `seen`: the same wire events
        // are stored in its directory too.
        let (next, next_clock) = live_dir(tmp.path(), "b");
        *live.lock().unwrap() = Some((next.clone(), next_clock));
        assert_eq!(
            sink.accept(batch(vec![signal(WALL_MS + 3_000, "ok")]))
                .unwrap(),
            1
        );
        assert_eq!(read_events(&next.meet_events()).unwrap().len(), 1);
        assert_eq!(read_events(&dir.meet_events()).unwrap().len(), 4);
    }
}
