use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn status_without_a_daemon_fails_with_a_hint() {
    let runtime = tempfile::tempdir().unwrap();
    Command::cargo_bin("recall")
        .unwrap()
        .arg("status")
        .env("XDG_RUNTIME_DIR", runtime.path())
        .assert()
        .failure()
        .stderr(contains("is `recall daemon` running?"));
}
