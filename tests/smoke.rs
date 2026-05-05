//! End-to-end and race tests against a real bare-repo fixture.
//! Spawn the built binary; serial run only (cargo build lock would race in parallel).
use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, Barrier};
use std::time::SystemTime;

// ---------- helpers ----------

fn pool_key() -> String {
    let pid = std::process::id();
    let nonce = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("wtp-test-{pid}-{nonce}")
}

fn pool_root(key: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
    home.join(".worktree-pool").join(key)
}

fn run_git(cwd: &Path, args: &[&str]) {
    let st = StdCommand::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(st.success(), "git {} failed in {}", args.join(" "), cwd.display());
}

fn run_git_root(args: &[&str]) {
    StdCommand::new("git").args(args).status().unwrap();
}

fn make_fixture(dir: &Path) -> PathBuf {
    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone",
        "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    run_git(&staging, &["config", "user.email", "t@t"]);
    run_git(&staging, &["config", "user.name", "t"]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    run_git(&staging, &["add", "README"]);
    run_git(&staging, &["commit", "--quiet", "-m", "initial"]);
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

struct Cleanup(String);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(pool_root(&self.0));
    }
}

fn init_pool(key: &str, bare: &Path) {
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", key, "init"])
        .arg("--source")
        .arg(bare)
        .args(["--max-slots", "4", "--groups", "ios,android"])
        .assert()
        .success();
}

fn acquire_dev(key: &str, name: &str) -> std::process::Output {
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", key, "acquire", "--name", name, "--group", "ios"])
        .output()
        .unwrap()
}

fn release(key: &str, name: &str) {
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", key, "release", "--name", name])
        .assert()
        .success();
}

// ---------- e2e tests ----------

#[test]
fn full_lifecycle() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-x");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(path.ends_with("/feat-x"));

    let ls = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "ls"])
        .output()
        .unwrap();
    let ls_text = String::from_utf8_lossy(&ls.stdout);
    assert!(ls_text.contains("feat-x"));
    assert!(ls_text.contains("held"));

    let inspect = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "inspect", "--name", "feat-x"])
        .output()
        .unwrap();
    let inspect_text = String::from_utf8_lossy(&inspect.stdout);
    assert!(inspect_text.contains("started_at"));
    assert!(inspect_text.contains("full_sha"));
    assert!(inspect_text.contains("group: ios"));

    release(&key, "feat-x");

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
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "--name", "build-1",
            "--commit", "main", "--group", "ios", "--unique-sha",
        ])
        .assert()
        .success();

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "--name", "build-2",
            "--commit", "main", "--group", "ios", "--unique-sha",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("full_sha"));
    assert!(stderr.contains("build-1"));

    // Dev acquire (no --unique-sha) is allowed.
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
    let _c = Cleanup(key.clone());
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "ls"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not initialized"));
}

#[test]
fn doctor_runs_without_pool() {
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .arg("doctor")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("worktree-pool doctor"));
    assert!(stdout.contains("arch:"));
    assert!(stdout.contains("git:"));
}

#[test]
fn unstick_lists_and_clears_stale() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // Plant a fake stale init mutex (mtime far in the past).
    let init_dir = pool_root(&key).join(".meta/init");
    std::fs::create_dir_all(&init_dir).unwrap();
    let stale = init_dir.join("ios-2.lock");
    std::fs::write(&stale, b"").unwrap();
    let one_year_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(365 * 86400);
    std::fs::File::open(&stale)
        .unwrap()
        .set_modified(one_year_ago)
        .unwrap();

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "unstick"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cleared"), "expected cleared in: {stdout}");
    assert!(!stale.exists(), "stale mutex should have been removed");
}

#[test]
fn ls_renders_with_git_status_for_held_only() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);
    acquire_dev(&key, "feat-x");

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "ls", "--git-status"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Header includes git-status columns
    assert!(stdout.contains("DIRTY"));
    assert!(stdout.contains("UNTRK"));
    assert!(stdout.contains("AHEAD"));
    // feat-x row has 0 dirty
    let row = stdout.lines().find(|l| l.starts_with("feat-x")).unwrap();
    assert!(row.contains(" 0 "), "feat-x row missing 0 dirty: {row}");
}

// ---------- race tests ----------

/// Recycled-slot bug regression: after release, re-acquire MUST land on the
/// same canonical N (warm `Library/`), not a fresh N.
#[test]
fn re_acquire_reuses_recycled_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // First acquire → release. Slot home should be ios-0.
    acquire_dev(&key, "feat-1");
    release(&key, "feat-1");

    // After release, ios-0 should exist as an idle dir.
    let ios_0 = pool_root(&key).join("ios-0");
    assert!(ios_0.exists(), "ios-0 should exist post-release");
    let warm_marker = pool_root(&key).join("ios-0/WARMTH_MARKER");
    std::fs::write(&warm_marker, b"warm").unwrap();

    // Re-acquire under a different name. Should pick up ios-0 (recycled).
    let out = acquire_dev(&key, "feat-2");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let new_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let new_warm_marker = PathBuf::from(&new_path).join("WARMTH_MARKER");
    assert!(
        new_warm_marker.exists(),
        "warmth marker missing — re-acquire created a fresh slot instead of recycling. \
         path: {new_path}"
    );
}

/// Parallel acquires of different names should each get a different slot
/// (no spurious "init mutex contended" — falls through to next N).
#[test]
fn parallel_acquires_get_different_slots() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // Three threads start simultaneously.
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for i in 0..3 {
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            Command::cargo_bin("worktree-pool")
                .unwrap()
                .args([
                    "--pool",
                    &key,
                    "acquire",
                    "--name",
                    &format!("dev-{i}"),
                    "--group",
                    "ios",
                ])
                .output()
                .unwrap()
        }));
    }
    let outs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = outs.iter().filter(|o| o.status.success()).count();
    assert_eq!(
        successes, 3,
        "all 3 parallel acquires should succeed (different slots); failures:\n{}",
        outs.iter()
            .filter(|o| !o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stderr).to_string())
            .collect::<Vec<_>>()
            .join("\n---\n")
    );

    // Each got a unique path.
    let paths: std::collections::HashSet<_> = outs
        .iter()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .collect();
    assert_eq!(paths.len(), 3, "duplicate slot paths: {paths:?}");
}

/// Capacity exhaustion: when all max_slots in a group are held, the next acquire
/// must fail loudly (with the slot table inline).
#[test]
fn acquire_capacity_exhaustion() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());

    // Smaller pool to make exhaustion fast.
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "init"])
        .arg("--source")
        .arg(&bare)
        .args(["--max-slots", "2", "--groups", "ios"])
        .assert()
        .success();

    acquire_dev(&key, "a");
    acquire_dev(&key, "b");
    let out = acquire_dev(&key, "c");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("held") || stderr.contains("in use"),
        "expected capacity error mentioning held/in-use; got: {stderr}"
    );
}

/// Parallel releases of two different held slots must both succeed
/// (release-mutex serializes the smallest-free-N picker).
#[test]
fn parallel_releases_different_names() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);
    acquire_dev(&key, "a");
    acquire_dev(&key, "b");

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for name in ["a", "b"] {
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            Command::cargo_bin("worktree-pool")
                .unwrap()
                .args(["--pool", &key, "release", "--name", name])
                .output()
                .unwrap()
        }));
    }
    let outs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for o in &outs {
        assert!(
            o.status.success(),
            "release failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }

    // Both back in pool as ios-0/ios-1 (some order).
    let ls = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "ls"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&ls.stdout);
    assert!(stdout.contains("ios-0"));
    assert!(stdout.contains("ios-1"));
    // No held slots.
    assert!(!stdout.lines().any(|l| l.starts_with("a ")));
    assert!(!stdout.lines().any(|l| l.starts_with("b ")));
}

/// Stale init mutex (planted with old mtime) must be reclaimed by acquire.
#[test]
fn stale_init_mutex_reclaimed_inline() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let init_dir = pool_root(&key).join(".meta/init");
    std::fs::create_dir_all(&init_dir).unwrap();
    let mutex_path = init_dir.join("ios-0.lock");
    std::fs::write(&mutex_path, b"").unwrap();
    let one_year_ago = SystemTime::now() - std::time::Duration::from_secs(365 * 86400);
    std::fs::File::open(&mutex_path)
        .unwrap()
        .set_modified(one_year_ago)
        .unwrap();

    // Acquire should reclaim and use ios-0.
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "--name", "post-reclaim",
            "--group", "ios",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("reclaiming stale init mutex"),
        "expected stale-reclaim warning; got: {stderr}"
    );
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(path.ends_with("/post-reclaim"));
}

/// Two parallel acquires with --unique-sha racing on the same SHA: at most one
/// should succeed. (Currently the same-SHA scan is not pool-globally serialized,
/// so this is best-effort; see TODO in README.)
#[test]
fn parallel_unique_sha_at_most_one_succeeds() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for i in 0..2 {
        let key = key.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            Command::cargo_bin("worktree-pool")
                .unwrap()
                .args([
                    "--pool", &key, "acquire", "--name", &format!("b-{i}"),
                    "--commit", "main", "--group", "ios", "--unique-sha",
                ])
                .output()
                .unwrap()
        }));
    }
    let outs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = outs.iter().filter(|o| o.status.success()).count();
    assert!(
        successes <= 1,
        "expected at most 1 success with --unique-sha race; got {successes}. \
         (Note: the scan is not pool-globally serialized; this can fail intermittently.)"
    );
}
