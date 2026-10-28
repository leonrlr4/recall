//! Screen recording beside the audio: `gpu-screen-recorder` writes
//! `.cache/screen.mkv` while recording, and [`finish`] remuxes it into
//! `screen.mp4` and works out where the video starts on the audio's
//! timeline.
//!
//! MKV because MP4 writes its index last, so a killed recorder would leave
//! an unplayable file; a cut-off MKV still plays up to where it stopped.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use recall_store::CaptureClock;

/// How long [`ScreenRecorder::stop`] waits after SIGINT before SIGKILL.
/// `gpu-screen-recorder` flushes its encoder on SIGINT, which takes well
/// under a second.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(20);
const GSR_ENV: &str = "RECALL_GSR";
const FFMPEG_ENV: &str = "RECALL_FFMPEG";
const GSR: &str = "gpu-screen-recorder";
const FFMPEG: &str = "ffmpeg";
const APP_DIR: &str = "recall";
const TOKEN_FILE: &str = "portal-token";

/// The programs and portal token the screen recording uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenTools {
    pub gsr: PathBuf,
    pub ffmpeg: PathBuf,
    /// The xdg-desktop-portal restore token `gpu-screen-recorder` reads and
    /// replaces on every run.
    pub token: PathBuf,
}

impl ScreenTools {
    /// `$RECALL_GSR` and `$RECALL_FFMPEG`, defaulting to the programs on
    /// `PATH`, and the token at `$XDG_STATE_HOME/recall/portal-token`
    /// (`~/.local/state/recall/portal-token`).
    pub fn from_env() -> Self {
        let program = |var: &str, default: &str| {
            PathBuf::from(std::env::var_os(var).unwrap_or_else(|| OsString::from(default)))
        };
        let state = dirs::state_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"));
        Self {
            gsr: program(GSR_ENV, GSR),
            ffmpeg: program(FFMPEG_ENV, FFMPEG),
            token: state.join(APP_DIR).join(TOKEN_FILE),
        }
    }
}

/// A running `gpu-screen-recorder`.
pub struct ScreenRecorder {
    child: Child,
    out: PathBuf,
    stop_timeout: Duration,
    /// Set once the child is seen to have exited.
    exited: Option<ExitStatus>,
}

impl ScreenRecorder {
    /// Starts recording the screen into `out` (an `.mkv`), with the portal
    /// session restored from `token`. Creates the directories of both. A
    /// missing or empty token makes the portal show its picker.
    pub fn start(program: &Path, out: &Path, token: &Path) -> Result<Self> {
        for path in [out, token] {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let mut command = Command::new(program);
        command
            .args(["-w", "portal", "-restore-portal-session", "yes"])
            .arg("-portal-session-token-filepath")
            .arg(token)
            .args(["-c", "mkv", "-k", "h264", "-fm", "content", "-f", "30"])
            .args(["-bm", "qp", "-q", "medium", "-cursor", "yes"])
            .args(["-write-first-frame-ts", "yes", "-o"])
            .arg(out)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        // SAFETY: `prctl` is async-signal-safe and touches no memory of
        // the parent.
        unsafe {
            command.pre_exec(|| {
                // A daemon killed outside `stop` would otherwise leave the
                // recorder filling the cache; SIGINT makes it finish the
                // file. The signal follows the spawning thread, which is
                // the recorder thread and outlives every recording.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGINT) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        Ok(Self {
            child,
            out: out.to_path_buf(),
            stop_timeout: STOP_TIMEOUT,
            exited: None,
        })
    }

    /// Replaces the 10 s [`ScreenRecorder::stop`] waits before SIGKILL.
    #[cfg(test)]
    fn with_stop_timeout(mut self, timeout: Duration) -> Self {
        self.stop_timeout = timeout;
        self
    }

    /// False once the recorder has exited.
    pub fn is_running(&mut self) -> bool {
        if self.exited.is_none() {
            match self.child.try_wait() {
                Ok(status) => self.exited = status,
                Err(err) => tracing::warn!(%err, "polling the screen recorder"),
            }
        }
        self.exited.is_none()
    }

    /// How the recorder exited, once [`ScreenRecorder::is_running`] has seen
    /// it exit.
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exited
    }

    /// Sends SIGINT, which makes `gpu-screen-recorder` finish the file, and
    /// waits for it to exit; after the stop timeout (10 s) it is killed. A
    /// killed recorder's MKV still plays up to its last cluster.
    pub fn stop(mut self) -> Result<()> {
        if !self.is_running() {
            return Ok(());
        }
        let pid = i32::try_from(self.child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .context("the screen recorder has no valid pid")?;
        if let Err(err) = rustix::process::kill_process(pid, rustix::process::Signal::INT) {
            tracing::warn!(%err, "sending SIGINT to the screen recorder");
        }
        let deadline = Instant::now() + self.stop_timeout;
        while Instant::now() < deadline {
            if !self.is_running() {
                return Ok(());
            }
            std::thread::sleep(STOP_POLL);
        }
        tracing::warn!(
            timeout_ms = self.stop_timeout.as_millis(),
            "the screen recorder ignored SIGINT; killing it"
        );
        self.child.kill().context("killing the screen recorder")?;
        self.child
            .wait()
            .context("waiting for the screen recorder")?;
        Ok(())
    }

    /// Stops the recorder and deletes what it wrote.
    pub fn discard(self) -> Result<()> {
        let out = self.out.clone();
        let stopped = self.stop();
        remove_files(&out)?;
        stopped
    }
}

/// `<mkv>.ts`, where `gpu-screen-recorder` writes the first frame's time.
pub fn ts_path(mkv: &Path) -> PathBuf {
    let mut path = mkv.as_os_str().to_owned();
    path.push(".ts");
    PathBuf::from(path)
}

/// Deletes `mkv` and its `.ts` sidecar; absent files are fine.
pub fn remove_files(mkv: &Path) -> Result<()> {
    for path in [mkv.to_path_buf(), ts_path(mkv)] {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", path.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Remuxes `cache_mkv` into `out_mp4` with `ffmpeg` (no re-encode, index
/// first so the browser can seek before it has the whole file) and returns
/// `screen_offset_ms`: how long after the capture origin in `clock` the
/// video's first frame came, positive when the video starts after the
/// audio. The offset is `None` without a clock or a readable `.ts`
/// sidecar; only the remux itself fails this.
pub fn finish(
    ffmpeg: &Path,
    cache_mkv: &Path,
    out_mp4: &Path,
    clock: Option<&CaptureClock>,
) -> Result<Option<i64>> {
    let output = Command::new(ffmpeg)
        .args(["-y", "-v", "error", "-i"])
        .arg(cache_mkv)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(out_mp4)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", ffmpeg.display()))?;
    if !output.status.success() {
        let _ = fs::remove_file(out_mp4);
        bail!(
            "{} exited with {}: {}",
            ffmpeg.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let Some(clock) = clock else {
        return Ok(None);
    };
    let ts = ts_path(cache_mkv);
    match first_frame_us(&ts) {
        Ok(Some(first)) => Ok(Some((first - clock.monotonic_us) / 1000)),
        Ok(None) => Ok(None),
        Err(err) => {
            tracing::warn!(err = format!("{err:#}"), "reading the first frame time");
            Ok(None)
        }
    }
}

/// The first frame's `CLOCK_MONOTONIC` microseconds from a `.ts` sidecar:
/// a header line, then `<monotonic_us>\t<realtime_us>`. A missing file is
/// `None`.
fn first_frame_us(ts: &Path) -> Result<Option<i64>> {
    let text = match fs::read_to_string(ts) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", ts.display())),
    };
    let value = text
        .lines()
        .skip(1)
        .find_map(|line| line.split('\t').next()?.trim().parse::<i64>().ok())
        .with_context(|| format!("no timestamp in {}", ts.display()))?;
    Ok(Some(value))
}

/// `"ok"` when a portal restore token is saved, `"picker"` when the file is
/// missing or empty and the next recording will show the portal's picker.
pub fn token_status(token: &Path) -> &'static str {
    match fs::metadata(token) {
        Ok(meta) if meta.len() > 0 => "ok",
        _ => "picker",
    }
}

/// Fake `gpu-screen-recorder` and `ffmpeg` scripts for tests.
#[cfg(test)]
pub mod fake {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::OnceLock;

    use super::*;

    /// Writes the video at once and the first frame time (5,250,000 µs) when
    /// SIGINT arrives, then exits.
    const GSR: &str = r#"#!/bin/sh
out=
while [ $# -gt 0 ]; do
  [ "$1" = -o ] && out=$2
  shift
done
printf video > "$out"
trap 'printf "monotonic_microsec\trealtime_microsec\n5250000\t1793170000250000\n" > "$out.ts"; exit 0' INT
while :; do sleep 0.02; done
"#;
    /// Ignores SIGINT.
    const STUBBORN_GSR: &str = r#"#!/bin/sh
trap '' INT
while :; do sleep 0.02; done
"#;
    /// Exits at once, as when the portal picker is cancelled.
    const EXITING_GSR: &str = "#!/bin/sh\nexit 1\n";
    /// Copies the input (after `-i`) to the output (the last argument).
    const FFMPEG: &str = r#"#!/bin/sh
in=
prev=
for arg; do
  [ "$prev" = -i ] && in=$arg
  prev=$arg
done
cp "$in" "$prev"
"#;
    const FAILING_FFMPEG: &str = "#!/bin/sh\necho 'Invalid data' >&2\nexit 1\n";

    pub struct Fakes {
        pub gsr: PathBuf,
        pub stubborn_gsr: PathBuf,
        pub exiting_gsr: PathBuf,
        pub ffmpeg: PathBuf,
        pub failing_ffmpeg: PathBuf,
    }

    /// Writes the scripts once per test process: a script written while
    /// another test thread forks could fail to exec with ETXTBSY.
    pub fn fakes() -> &'static Fakes {
        static FAKES: OnceLock<(tempfile::TempDir, Fakes)> = OnceLock::new();
        let (_, fakes) = FAKES.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let write = |name: &str, body: &str| {
                let path = dir.path().join(name);
                fs::write(&path, body).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                path
            };
            let fakes = Fakes {
                gsr: write("gsr", GSR),
                stubborn_gsr: write("stubborn-gsr", STUBBORN_GSR),
                exiting_gsr: write("exiting-gsr", EXITING_GSR),
                ffmpeg: write("ffmpeg", FFMPEG),
                failing_ffmpeg: write("failing-ffmpeg", FAILING_FFMPEG),
            };
            (dir, fakes)
        });
        fakes
    }

    /// Tools using the fakes, with the token in `dir`.
    pub fn tools(dir: &Path) -> ScreenTools {
        let fakes = fakes();
        ScreenTools {
            gsr: fakes.gsr.clone(),
            ffmpeg: fakes.ffmpeg.clone(),
            token: dir.join("portal-token"),
        }
    }

    /// Waits up to 5 s for `path` to exist.
    pub fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{fakes, wait_for};
    use super::*;

    #[test]
    fn stop_sends_sigint_and_keeps_output() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join(".cache/screen.mkv");
        let token = tmp.path().join("state/portal-token");
        let mut rec = ScreenRecorder::start(&fakes().gsr, &out, &token).unwrap();
        wait_for(&out);
        assert!(rec.is_running());

        rec.stop().unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"video");
        // Written by the SIGINT trap, so the recorder was not killed.
        assert!(ts_path(&out).is_file());
        assert!(token.parent().unwrap().is_dir());
    }

    #[test]
    fn stubborn_child_is_killed_after_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("screen.mkv");
        let rec = ScreenRecorder::start(&fakes().stubborn_gsr, &out, &tmp.path().join("token"))
            .unwrap()
            .with_stop_timeout(Duration::from_millis(200));
        // Give the shell time to install its trap.
        std::thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        rec.stop().unwrap();
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(200), "{took:?}");
        assert!(took < Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn early_exit_is_seen() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rec = ScreenRecorder::start(
            &fakes().exiting_gsr,
            &tmp.path().join("screen.mkv"),
            &tmp.path().join("token"),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rec.is_running() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(rec.exit_status().and_then(|s| s.code()), Some(1));
        rec.stop().unwrap();
    }

    #[test]
    fn discard_deletes_video_and_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("screen.mkv");
        let rec = ScreenRecorder::start(&fakes().gsr, &out, &tmp.path().join("token")).unwrap();
        wait_for(&out);
        rec.discard().unwrap();
        assert!(!out.exists());
        assert!(!ts_path(&out).exists());
    }

    #[test]
    fn finish_remuxes_and_computes_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let mkv = tmp.path().join("screen.mkv");
        let mp4 = tmp.path().join("screen.mp4");
        fs::write(&mkv, b"video").unwrap();
        fs::write(
            ts_path(&mkv),
            "monotonic_microsec\trealtime_microsec\n5250000\t1793170000250000\n",
        )
        .unwrap();
        let clock = CaptureClock {
            wall_ms: 1_793_170_000_000,
            monotonic_us: 5_000_000,
        };

        let offset = finish(&fakes().ffmpeg, &mkv, &mp4, Some(&clock)).unwrap();
        assert_eq!(offset, Some(250));
        assert_eq!(fs::read(&mp4).unwrap(), b"video");

        // Without a clock or a sidecar the video is kept, offset unknown.
        assert_eq!(finish(&fakes().ffmpeg, &mkv, &mp4, None).unwrap(), None);
        fs::remove_file(ts_path(&mkv)).unwrap();
        assert_eq!(
            finish(&fakes().ffmpeg, &mkv, &mp4, Some(&clock)).unwrap(),
            None
        );
    }

    #[test]
    fn failed_remux_is_an_error_and_leaves_no_mp4() {
        let tmp = tempfile::tempdir().unwrap();
        let mkv = tmp.path().join("screen.mkv");
        let mp4 = tmp.path().join("screen.mp4");
        fs::write(&mkv, b"video").unwrap();
        let err = finish(&fakes().failing_ffmpeg, &mkv, &mp4, None).unwrap_err();
        assert!(format!("{err:#}").contains("Invalid data"), "{err:#}");
        assert!(!mp4.exists());
    }

    #[test]
    fn token_status_reports_picker_for_empty_token() {
        let tmp = tempfile::tempdir().unwrap();
        let token = tmp.path().join("portal-token");
        assert_eq!(token_status(&token), "picker");
        fs::write(&token, "").unwrap();
        assert_eq!(token_status(&token), "picker");
        fs::write(&token, "a1b2c3").unwrap();
        assert_eq!(token_status(&token), "ok");
    }
}
