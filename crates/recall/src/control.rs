//! The control socket: newline-delimited JSON over a Unix socket at
//! `$XDG_RUNTIME_DIR/recall/control.sock`. The CLI writes one [`Command`] per
//! line and reads one [`Reply`] line back; a connection may send several.
//!
//! `{"command":"status"}` answers `{"reply":"status","phase":"recording",...}`;
//! the other commands answer `{"reply":"ok"}`, or `{"reply":"error",...}` when
//! the recorder is gone or the line does not parse.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, mpsc};

use anyhow::{Context, Result, anyhow};
use recall_audio::state::DetectEvent;
use recall_server::{ControlCommand, StatusSnapshot};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

const SOCKET: &str = "control.sock";
const APP_DIR: &str = "recall";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Start,
    Stop,
    Discard,
    Status,
}

impl Command {
    /// The recorder event this command sends; `None` for [`Command::Status`].
    pub fn event(self) -> Option<DetectEvent> {
        match self {
            Command::Start => Some(DetectEvent::HotkeyStart),
            Command::Stop => Some(DetectEvent::HotkeyStop),
            Command::Discard => Some(DetectEvent::HotkeyDiscard),
            Command::Status => None,
        }
    }
}

impl From<ControlCommand> for Command {
    fn from(cmd: ControlCommand) -> Self {
        match cmd {
            ControlCommand::Start => Command::Start,
            ControlCommand::Stop => Command::Stop,
            ControlCommand::Discard => Command::Discard,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    Status(StatusSnapshot),
    Error { message: String },
}

/// `$XDG_RUNTIME_DIR/recall`, or `<tmp>/recall-<user>` when there is no
/// runtime dir.
pub fn runtime_dir() -> PathBuf {
    match dirs::runtime_dir() {
        Some(dir) => dir.join(APP_DIR),
        None => {
            let user = std::env::var("USER").unwrap_or_default();
            std::env::temp_dir().join(format!("{APP_DIR}-{user}"))
        }
    }
}

/// Creates `dir` readable only by the user: the control socket accepts
/// commands from anyone who can reach it.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    // `mode` applies only to directories it creates; tighten an older one.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", dir.display()))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join(SOCKET)
}

/// Accepts connections on `path` until the task is dropped. Creates the
/// parent directory and replaces a stale socket file left by a daemon that
/// did not exit cleanly.
pub async fn serve(
    path: &Path,
    tx: mpsc::Sender<DetectEvent>,
    status: Arc<RwLock<StatusSnapshot>>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", path.display()));
        }
        _ => {}
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    loop {
        let (stream, _) = listener.accept().await.context("accepting a client")?;
        let tx = tx.clone();
        let status = status.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(stream, tx, status).await {
                tracing::warn!(err = format!("{err:#}"), "control client");
            }
        });
    }
}

async fn handle(
    stream: tokio::net::UnixStream,
    tx: mpsc::Sender<DetectEvent>,
    status: Arc<RwLock<StatusSnapshot>>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = tokio::io::BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let reply = match serde_json::from_str::<Command>(&line) {
            Ok(cmd) => answer(cmd, &tx, &status),
            Err(err) => Reply::Error {
                message: format!("bad command: {err}"),
            },
        };
        let mut out = serde_json::to_vec(&reply)?;
        out.push(b'\n');
        write.write_all(&out).await?;
    }
    Ok(())
}

fn answer(cmd: Command, tx: &mpsc::Sender<DetectEvent>, status: &RwLock<StatusSnapshot>) -> Reply {
    match cmd.event() {
        None => Reply::Status(
            status
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        ),
        Some(event) => match tx.send(event) {
            Ok(()) => Reply::Ok,
            Err(_) => Reply::Error {
                message: "the recorder has stopped".into(),
            },
        },
    }
}

/// Sends `cmd` to the running daemon and waits for its reply.
pub fn send(cmd: Command) -> Result<Reply> {
    send_to(&socket_path(), cmd)
}

/// [`send`] to the socket at `path`.
pub fn send_to(path: &Path, cmd: Command) -> Result<Reply> {
    let mut stream = UnixStream::connect(path).with_context(|| {
        format!(
            "connecting to {} (is `recall daemon` running?)",
            path.display()
        )
    })?;
    let mut line = serde_json::to_vec(&cmd)?;
    line.push(b'\n');
    stream.write_all(&line).context("sending the command")?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .context("reading the reply")?;
    if reply.is_empty() {
        return Err(anyhow!("the daemon closed the connection without a reply"));
    }
    serde_json::from_str(&reply).with_context(|| format!("parsing the reply {reply:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> StatusSnapshot {
        StatusSnapshot {
            phase: "recording".into(),
            since_ms: 1_761_546_600_000,
            source_app: Some("brave".into()),
            mic_peak: 0.5,
            sys_peak: 0.25,
        }
    }

    #[tokio::test]
    async fn control_round_trip_over_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall/control.sock");
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(RwLock::new(snapshot()));
        let server = {
            let path = path.clone();
            tokio::spawn(async move { serve(&path, tx, status).await })
        };
        while !path.exists() {
            assert!(!server.is_finished(), "serve exited early");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let replies = tokio::task::spawn_blocking(move || {
            [
                Command::Status,
                Command::Start,
                Command::Stop,
                Command::Discard,
            ]
            .map(|cmd| send_to(&path, cmd).unwrap())
        })
        .await
        .unwrap();

        assert_eq!(
            replies,
            [Reply::Status(snapshot()), Reply::Ok, Reply::Ok, Reply::Ok]
        );
        let events: Vec<DetectEvent> = rx.try_iter().collect();
        assert_eq!(
            events,
            [
                DetectEvent::HotkeyStart,
                DetectEvent::HotkeyStop,
                DetectEvent::HotkeyDiscard
            ]
        );
        server.abort();
    }

    #[test]
    fn wire_format_is_tagged_json() {
        assert_eq!(
            serde_json::to_string(&Command::Status).unwrap(),
            r#"{"command":"status"}"#
        );
        assert_eq!(
            serde_json::to_string(&Reply::Ok).unwrap(),
            r#"{"reply":"ok"}"#
        );
        let status = serde_json::to_value(Reply::Status(snapshot())).unwrap();
        assert_eq!(status["reply"], "status");
        assert_eq!(status["phase"], "recording");
        assert_eq!(status["source_app"], "brave");
    }

    #[tokio::test]
    async fn malformed_line_gets_an_error_reply() {
        let (tx, _rx) = mpsc::channel();
        let status = Arc::new(RwLock::new(snapshot()));
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(handle(server, tx, status));
        let (read, mut write) = client.into_split();
        write.write_all(b"{\"command\":\"jump\"}\n").await.unwrap();
        let line = tokio::io::BufReader::new(read)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        let reply: Reply = serde_json::from_str(&line).unwrap();
        assert!(matches!(reply, Reply::Error { .. }), "{reply:?}");
    }

    #[test]
    fn socket_lives_in_the_runtime_dir() {
        assert!(socket_path().ends_with("recall/control.sock"));
    }
}
