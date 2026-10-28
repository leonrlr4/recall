//! Google Meet events sent by the browser extension and stored as
//! `meet-events.jsonl` in the meeting directory.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// What happened in the call. `participant` is the extension's stable id for
/// a remote stream, e.g. `"csrc:12345"`. A kind this version does not know
/// (from a newer extension) parses as `Unknown`, so it costs only itself,
/// not the batch it came in; `Unknown` events are never stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MeetEventKind {
    SpeakingStart {
        participant: String,
    },
    SpeakingStop {
        participant: String,
    },
    Name {
        participant: String,
        name: String,
    },
    Roster {
        names: Vec<String>,
    },
    Status {
        status: String,
    },
    #[serde(other)]
    Unknown,
}

/// One event. `t` is Unix milliseconds on the wire and milliseconds from the
/// recording origin in `meet-events.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetEvent {
    pub t: i64,
    #[serde(flatten)]
    pub kind: MeetEventKind,
}

/// The body of `POST /api/meet/events`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeetBatch {
    pub meeting_code: String,
    pub events: Vec<MeetEvent>,
}

/// Appends `events` to `path` as JSON Lines, creating the file if needed.
pub fn append_events(path: &Path, events: &[MeetEvent]) -> Result<()> {
    let mut buf = Vec::new();
    for event in events {
        serde_json::to_writer(&mut buf, event)?;
        buf.push(b'\n');
    }
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(&buf))
        .with_context(|| format!("appending to {}", path.display()))
}

/// Reads every event in `path`, in file order. A missing file has no events;
/// a line that does not parse is skipped with a warning, so one torn write
/// does not lose the rest, and an `Unknown` event is skipped silently.
pub fn read_events(path: &Path) -> Result<Vec<MeetEvent>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut events = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(MeetEvent {
                kind: MeetEventKind::Unknown,
                ..
            }) => {}
            Ok(event) => events.push(event),
            Err(e) => {
                tracing::warn!("skipping {} line {}: {e}", path.display(), n + 1);
            }
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_BATCH: &str = r#"{"meeting_code": "abc-defg-hij",
 "events": [
   {"t": 1793170000123, "kind": "speaking_start", "participant": "csrc:12345"},
   {"t": 1793170004456, "kind": "speaking_stop", "participant": "csrc:12345"},
   {"t": 1793170000500, "kind": "name", "participant": "csrc:12345", "name": "Tom Lin"},
   {"t": 1793170000600, "kind": "roster", "names": ["Tom Lin", "Amy Chen"]},
   {"t": 1793170300000, "kind": "status", "status": "signal_lost"}]}"#;

    #[test]
    fn meet_event_round_trips_wire_format() {
        let batch: MeetBatch = serde_json::from_str(SPEC_BATCH).unwrap();
        assert_eq!(batch.meeting_code, "abc-defg-hij");
        assert_eq!(
            batch.events,
            vec![
                MeetEvent {
                    t: 1_793_170_000_123,
                    kind: MeetEventKind::SpeakingStart {
                        participant: "csrc:12345".into()
                    },
                },
                MeetEvent {
                    t: 1_793_170_004_456,
                    kind: MeetEventKind::SpeakingStop {
                        participant: "csrc:12345".into()
                    },
                },
                MeetEvent {
                    t: 1_793_170_000_500,
                    kind: MeetEventKind::Name {
                        participant: "csrc:12345".into(),
                        name: "Tom Lin".into()
                    },
                },
                MeetEvent {
                    t: 1_793_170_000_600,
                    kind: MeetEventKind::Roster {
                        names: vec!["Tom Lin".into(), "Amy Chen".into()]
                    },
                },
                MeetEvent {
                    t: 1_793_170_300_000,
                    kind: MeetEventKind::Status {
                        status: "signal_lost".into()
                    },
                },
            ]
        );
        let original: serde_json::Value = serde_json::from_str(SPEC_BATCH).unwrap();
        assert_eq!(serde_json::to_value(&batch).unwrap(), original);
    }

    #[test]
    fn unknown_kind_parses_without_failing_the_batch() {
        let batch: MeetBatch = serde_json::from_str(
            r#"{"meeting_code": "abc-defg-hij", "events": [
                {"t": 1, "kind": "hand_raised", "participant": "csrc:1", "extra": [1, 2]},
                {"t": 2, "kind": "status", "status": "ok"}]}"#,
        )
        .unwrap();
        assert_eq!(batch.events[0].kind, MeetEventKind::Unknown);
        assert_eq!(batch.events[0].t, 1);
        assert_eq!(
            batch.events[1].kind,
            MeetEventKind::Status {
                status: "ok".into()
            }
        );
        // A known kind with missing fields is still an error.
        assert!(serde_json::from_str::<MeetEvent>(r#"{"t": 1, "kind": "name"}"#).is_err());
        assert!(serde_json::from_str::<MeetEvent>(r#"{"t": 1}"#).is_err());
    }

    #[test]
    fn read_events_skips_bad_lines_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("meet-events.jsonl");
        assert!(read_events(&path).unwrap().is_empty());

        let first = MeetEvent {
            t: 0,
            kind: MeetEventKind::SpeakingStart {
                participant: "csrc:1".into(),
            },
        };
        let second = MeetEvent {
            t: 4_000,
            kind: MeetEventKind::SpeakingStop {
                participant: "csrc:1".into(),
            },
        };
        append_events(&path, std::slice::from_ref(&first)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"t\": 1, \"kind\": \"unknown\"}\nnot json\n\n")
            .unwrap();
        append_events(&path, std::slice::from_ref(&second)).unwrap();

        assert_eq!(read_events(&path).unwrap(), vec![first, second]);
    }
}
