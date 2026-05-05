//! End-to-end test against a real bare-repo fixture.
//! These spawn the built binary; they're slow relative to unit tests but cover the full flow.
use assert_cmd::Command;
use std::path::PathBuf;
use std::process::Command as StdCommand;

fn pool_key() -> String {
    // Per-test key so concurrent test runs don't collide on `~/.worktree-pool/<key>/`.
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("wtp-test-{pid}-{nonce}")
}

fn pool_root(key: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
    home.join(".worktree-pool").join(key)
}

/// Build a bare repo in `dir` with one commit on `main`. Returns the bare path.
fn make_fixture(dir: &std::path::Path) -> PathBuf {
    let bare = dir.join("source.git");
    StdCommand::new("git")
        .args(["init", "--quiet", "--bare"])
        .arg(&bare)
        .status()
        .unwrap();
    let staging = dir.join("staging");
    StdCommand::new("git")
        .args(["clone", "--quiet"])
        .arg(&bare)
        .arg(&staging)
        .status()
        .unwrap();
    StdCommand::new("git")
        .args(["config", "user.email", "t@t"])
        .current_dir(&staging)
        .status()
        .unwrap();
    StdCommand::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(&staging)
        .status()
        .unwrap();
    std::fs::write(staging.join("README"), b"hi").unwrap();
    StdCommand::new("git")
        .args(["add", "README"])
        .current_dir(&staging)
        .status()
        .unwrap();
    StdCommand::new("git")
        .args(["commit", "--quiet", "-m", "initial"])
        .current_dir(&staging)
        .status()
        .unwrap();
    StdCommand::new("git")
        .args(["push", "--quiet", "-u", "origin", "main"])
        .current_dir(&staging)
        .status()
        .unwrap();
    bare
}

struct Cleanup(String);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(pool_root(&self.0));
    }
}

#[test]
fn full_lifecycle() {
    let key = pool_key();
    let _cleanup = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());

    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "init"])
        .arg("--source")
        .arg(&bare)
        .args(["--max-slots", "3", "--groups", "ios"])
        .assert()
        .success();

    // Acquire dev (no --unique-sha).
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "feat-x", "--group", "ios"])
        .output()
        .unwrap();
    assert!(out.status.success(), "acquire failed: {}", String::from_utf8_lossy(&out.stderr));
    let path_line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(path_line.ends_with("/feat-x"), "unexpected acquire stdout: {path_line:?}");

    // ls should show feat-x as held.
    let ls = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "ls"])
        .output()
        .unwrap();
    let ls_text = String::from_utf8_lossy(&ls.stdout);
    assert!(ls_text.contains("feat-x"), "ls didn't list feat-x:\n{ls_text}");
    assert!(ls_text.contains("held"), "ls didn't mark feat-x held");

    // Release.
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "release", "--name", "feat-x"])
        .assert()
        .success();

    // Idempotent re-release.
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "release", "--name", "feat-x"])
        .assert()
        .success();
}

#[test]
fn unique_sha_refuses_second_acquire() {
    let key = pool_key();
    let _cleanup = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());

    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "init"])
        .arg("--source")
        .arg(&bare)
        .args(["--max-slots", "3", "--groups", "ios"])
        .assert()
        .success();

    // First acquire with --unique-sha.
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "--name", "build-1",
            "--commit", "main", "--group", "ios", "--unique-sha",
        ])
        .assert()
        .success();

    // Second acquire of same SHA with --unique-sha must fail.
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "--name", "build-2",
            "--commit", "main", "--group", "ios", "--unique-sha",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "second --unique-sha should have failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("full_sha"), "unexpected stderr: {stderr}");
    assert!(stderr.contains("build-1"), "stderr should name holder: {stderr}");

    // But a dev acquire (no --unique-sha) IS allowed at the same SHA.
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "--name", "dev-foo",
            "--commit", "main", "--group", "ios",
        ])
        .assert()
        .success();
}

#[test]
fn refuses_uninitialized_pool() {
    let key = pool_key();
    let _cleanup = Cleanup(key.clone());

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "ls"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not initialized"), "expected init hint, got: {stderr}");
}
