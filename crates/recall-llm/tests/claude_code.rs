//! The Claude Code backend against small shell scripts standing in for
//! `claude`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use recall_llm::{ClaudeCode, Llm};

/// Writes an executable `sh` script named `claude` into `dir`.
fn fake(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("claude");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn backend(command: &Path, cwd: &Path, timeout: Duration) -> ClaudeCode {
    ClaudeCode::new(command.to_str().unwrap(), cwd.to_path_buf(), None, timeout)
}

#[tokio::test]
async fn returns_result_and_reports_the_model() {
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // Saves stdin, the arguments and the working directory, then prints a
    // result the way `claude -p --output-format json` does.
    let script = fake(
        bin.path(),
        r#"cat > stdin.txt
printf '%s\n' "$@" > args.txt
pwd > cwd.txt
echo '{"type":"result","subtype":"success","is_error":false,"result":"{\"one_line\":\"ok\"}","modelUsage":{"claude-haiku-5-5":{"outputTokens":3},"claude-opus-5-5":{"outputTokens":400}}}'"#,
    );
    let cc = backend(&script, work.path(), Duration::from_secs(10));

    let reply = cc.complete("SYSTEM", "[00:00] Me: TRL6\n").await.unwrap();

    assert_eq!(reply, r#"{"one_line":"ok"}"#);
    assert_eq!(cc.name(), "claude-code/claude-opus-5-5");
    let read = |f: &str| std::fs::read_to_string(work.path().join(f)).unwrap();
    assert_eq!(read("stdin.txt"), "[00:00] Me: TRL6\n");
    assert!(!read("args.txt").contains("TRL6"), "transcript not in argv");
    assert!(read("args.txt").contains("SYSTEM"));
    assert_eq!(
        Path::new(read("cwd.txt").trim()).canonicalize().unwrap(),
        work.path().canonicalize().unwrap()
    );
}

#[tokio::test]
async fn nonzero_exit_is_an_error_quoting_the_reason() {
    let bin = tempfile::tempdir().unwrap();
    let script = fake(
        bin.path(),
        "cat > /dev/null\necho 'Invalid API key. Please run /login' >&2\nexit 1",
    );
    let cc = backend(&script, bin.path(), Duration::from_secs(10));

    let err = cc.complete("s", "u").await.unwrap_err();

    let text = format!("{err:#}");
    assert!(text.contains("exited with"), "{text}");
    assert!(text.contains("Please run /login"), "{text}");
}

#[tokio::test]
async fn error_result_is_an_error() {
    let bin = tempfile::tempdir().unwrap();
    let script = fake(
        bin.path(),
        r#"cat > /dev/null
echo '{"type":"result","subtype":"error_max_turns","is_error":true,"result":"stopped"}'"#,
    );
    let cc = backend(&script, bin.path(), Duration::from_secs(10));

    let text = format!("{:#}", cc.complete("s", "u").await.unwrap_err());

    assert!(text.contains("error_max_turns"), "{text}");
    assert!(text.contains("stopped"), "{text}");
}

#[tokio::test]
async fn output_that_is_not_json_is_an_error() {
    let bin = tempfile::tempdir().unwrap();
    let script = fake(bin.path(), "cat > /dev/null\necho 'plain text'");
    let cc = backend(&script, bin.path(), Duration::from_secs(10));

    let text = format!("{:#}", cc.complete("s", "u").await.unwrap_err());

    assert!(text.contains("not a JSON result"), "{text}");
    assert!(text.contains("plain text"), "{text}");
}

#[tokio::test]
async fn missing_command_is_an_error() {
    let bin = tempfile::tempdir().unwrap();
    let cc = backend(
        &bin.path().join("no-such-claude"),
        bin.path(),
        Duration::from_secs(10),
    );

    let text = format!("{:#}", cc.complete("s", "u").await.unwrap_err());

    assert!(text.contains("starting"), "{text}");
}

#[tokio::test]
async fn timeout_kills_the_run_and_what_it_started() {
    let bin = tempfile::tempdir().unwrap();
    let marker = bin.path().join("child.pid");
    // A background child that would outlive the script if only the script
    // were killed.
    let script = fake(
        bin.path(),
        &format!("sleep 30 &\necho $! > {}\nsleep 30", marker.display()),
    );
    let cc = backend(&script, bin.path(), Duration::from_millis(500));

    let started = Instant::now();
    let text = format!("{:#}", cc.complete("s", "u").await.unwrap_err());

    assert!(text.contains("did not finish within"), "{text}");
    assert!(started.elapsed() < Duration::from_secs(10));
    let pid = std::fs::read_to_string(&marker).unwrap();
    let pid = pid.trim();
    // Give the kernel a moment to deliver the signal.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let alive = std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "background child {pid} survived the timeout");
}
