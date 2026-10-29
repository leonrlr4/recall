//! Desktop notifications and the status file read by bars and scripts.
//!
//! Notifications are best effort: each is shown from its own thread, so a
//! slow or missing notification daemon never stalls the recorder, and a
//! failure is only logged.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use notify_rust::Notification;
use recall_server::StatusSnapshot;

use crate::control::{create_private_dir, runtime_dir};
use crate::worker::percent_encode;

const APP_NAME: &str = "recall";
const STATE_FILE: &str = "state.json";
/// The action a notification server invokes when the notification itself is
/// clicked.
const DEFAULT_ACTION: &str = "default";

/// The notification each meeting last showed, by meeting id, so the next
/// one replaces it: a meeting shows one notification that moves from
/// recording to saved to summary ready, not a stack of three.
static SLOTS: LazyLock<Mutex<HashMap<String, Slot>>> = LazyLock::new(Mutex::default);

#[derive(Default)]
struct Slot {
    /// The server's id for the notification shown, once it is known.
    id: Option<u32>,
    /// Bumped by each notification for the meeting. A replaced notification
    /// keeps the same server id, so only the newest one's thread acts on a
    /// click.
    generation: u64,
}

pub fn recording_started(meeting: &str, title: &str, app: &str, port: u16) {
    show(Note {
        summary: "Recording",
        body: format!("{title} · {app}"),
        url: Some(home_url(port)),
        meeting: Some(meeting),
    });
}

/// The recording is saved and queued for transcription and the summary.
pub fn recording_saved(meeting: &str, title: &str, duration_ms: u32, port: u16) {
    show(Note {
        summary: "Recording saved",
        body: format!(
            "{title} · {}\nTranscribing, then summarizing",
            format_duration(duration_ms)
        ),
        url: Some(meeting_url(port, meeting)),
        meeting: Some(meeting),
    });
}

pub fn summary_ready(meeting: &str, title: &str, port: u16) {
    show(Note {
        summary: "Summary ready",
        body: title.to_owned(),
        url: Some(meeting_url(port, meeting)),
        meeting: Some(meeting),
    });
}

/// Processing `meeting` failed at `step`; the meeting page can retry it.
pub fn processing_failed(meeting: &str, title: &str, step: &str, port: u16) {
    show(Note {
        summary: "recall failed",
        body: format!("{title}: {step} failed"),
        url: Some(meeting_url(port, meeting)),
        meeting: Some(meeting),
    });
}

/// `what` failed for `title` (the app being recorded).
pub fn failed(title: &str, what: &str) {
    show(Note {
        summary: "recall failed",
        body: format!("{title}: {what} failed"),
        url: None,
        meeting: None,
    });
}

/// The screen recording failed to start or stopped early; the audio goes on.
pub fn screen_failed(reason: &str) {
    show(Note {
        summary: "Screen recording stopped",
        body: format!("Audio is still recording. {reason}"),
        url: None,
        meeting: None,
    });
}

/// Shown when a recording starts with little free space in `dir`.
pub fn low_space(dir: &Path, free_bytes: u64) {
    show(Note {
        summary: "Low disk space",
        body: format!(
            "{:.1} GB free in {}; recording stops when the disk is full",
            free_bytes as f64 / 1e9,
            dir.display()
        ),
        url: None,
        meeting: None,
    });
}

fn home_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

fn meeting_url(port: u16, meeting: &str) -> String {
    format!("http://127.0.0.1:{port}/m/{}", percent_encode(meeting))
}

/// `34 s`, `12 min`, `1 h 05 min`.
fn format_duration(ms: u32) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        _ => format!("{} h {:02} min", secs / 3600, secs % 3600 / 60),
    }
}

struct Note<'a> {
    summary: &'a str,
    body: String,
    /// Opened with `xdg-open` when the notification is clicked.
    url: Option<String>,
    /// The meeting whose earlier notification this one replaces.
    meeting: Option<&'a str>,
}

fn show(note: Note) {
    let mut notification = Notification::new();
    notification
        .appname(APP_NAME)
        .summary(note.summary)
        .body(&note.body);
    if note.url.is_some() {
        notification.action(DEFAULT_ACTION, "Open");
    }
    let slot = note.meeting.map(|meeting| {
        let mut slots = lock_slots();
        let slot = slots.entry(meeting.to_owned()).or_default();
        slot.generation += 1;
        if let Some(id) = slot.id {
            notification.id(id);
        }
        (meeting.to_owned(), slot.generation)
    });
    let open_url = note.url;
    let spawned = std::thread::Builder::new()
        .name("recall-notify".into())
        .spawn(move || {
            let handle = match notification.show() {
                Ok(handle) => handle,
                Err(err) => {
                    tracing::warn!(%err, "showing a notification");
                    return;
                }
            };
            if let Some((meeting, generation)) = &slot {
                set_id(meeting, *generation, handle.id());
            }
            let Some(url) = open_url else { return };
            handle.wait_for_action(|action| {
                let replaced = slot
                    .as_ref()
                    .is_some_and(|(meeting, generation)| !is_current(meeting, *generation));
                if action == DEFAULT_ACTION
                    && !replaced
                    && let Err(err) = std::process::Command::new("xdg-open").arg(&url).spawn()
                {
                    tracing::warn!(%err, "running xdg-open");
                }
            });
        });
    if let Err(err) = spawned {
        tracing::warn!(%err, "spawning the notification thread");
    }
}

/// Records `id` as the server's id for `meeting`, unless a newer
/// notification for it has been sent since `generation`.
fn set_id(meeting: &str, generation: u64, id: u32) {
    if let Some(slot) = lock_slots().get_mut(meeting)
        && slot.generation == generation
    {
        slot.id = Some(id);
    }
}

fn is_current(meeting: &str, generation: u64) -> bool {
    lock_slots()
        .get(meeting)
        .is_some_and(|slot| slot.generation == generation)
}

fn lock_slots() -> MutexGuard<'static, HashMap<String, Slot>> {
    SLOTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `$XDG_RUNTIME_DIR/recall/state.json`.
pub fn state_path() -> PathBuf {
    runtime_dir().join(STATE_FILE)
}

/// Writes `snapshot` to `path` as JSON through a temporary file and a rename,
/// so a reader never sees a partial file. The parent directory is created
/// with mode 0700.
pub fn write_state(path: &Path, snapshot: &StatusSnapshot) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    create_private_dir(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    serde_json::to_writer(&mut tmp, snapshot)?;
    tmp.flush()?;
    tmp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn durations_are_short_and_readable() {
        assert_eq!(format_duration(34_400), "34 s");
        assert_eq!(format_duration(754_000), "12 min");
        assert_eq!(format_duration(3_900_000), "1 h 05 min");
    }

    #[test]
    fn meeting_urls_encode_the_id() {
        assert_eq!(
            meeting_url(7474, "2026-10-29 12:03 Meeting 12:03"),
            "http://127.0.0.1:7474/m/2026-10-29%2012%3A03%20Meeting%2012%3A03"
        );
    }

    #[test]
    fn write_state_replaces_the_file_in_a_private_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("recall/state.json");
        let mut snapshot = StatusSnapshot {
            phase: "idle".into(),
            since_ms: 1,
            source_app: None,
            mic_peak: 0.0,
            sys_peak: 0.0,
            meet: None,
            screen: None,
        };
        write_state(&path, &snapshot).unwrap();
        snapshot.phase = "recording".into();
        write_state(&path, &snapshot).unwrap();

        let read: StatusSnapshot = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, snapshot);
        let mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }
}
