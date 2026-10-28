//! Desktop notifications and the status file read by bars and scripts.
//!
//! Notifications are best effort: each is shown from its own thread, so a
//! slow or missing notification daemon never stalls the recorder, and a
//! failure is only logged.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use notify_rust::Notification;
use recall_server::StatusSnapshot;

use crate::control::{create_private_dir, runtime_dir};

const APP_NAME: &str = "recall";
const STATE_FILE: &str = "state.json";
/// The action a notification server invokes when the notification itself is
/// clicked.
const DEFAULT_ACTION: &str = "default";

pub fn recording_started(app: &str) {
    show("Recording", &format!("Recording audio from {app}"), None);
}

pub fn processing(title: &str) {
    show("Processing", title, None);
}

/// Clicking the notification opens `url` with `xdg-open`.
pub fn summary_ready(title: &str, url: &str) {
    show("Summary ready", title, Some(url.to_owned()));
}

/// `what` failed for `title` (a meeting title, or the app being recorded).
pub fn failed(title: &str, what: &str) {
    show("recall failed", &format!("{title}: {what} failed"), None);
}

/// Shown when a recording starts with little free space in `dir`.
pub fn low_space(dir: &Path, free_bytes: u64) {
    show(
        "Low disk space",
        &format!(
            "{:.1} GB free in {}; recording stops when the disk is full",
            free_bytes as f64 / 1e9,
            dir.display()
        ),
        None,
    );
}

fn show(summary: &str, body: &str, open_url: Option<String>) {
    let mut notification = Notification::new();
    notification.appname(APP_NAME).summary(summary).body(body);
    if open_url.is_some() {
        notification.action(DEFAULT_ACTION, "Open");
    }
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
            let Some(url) = open_url else { return };
            handle.wait_for_action(|action| {
                if action == DEFAULT_ACTION
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
