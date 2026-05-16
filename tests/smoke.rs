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
    // Mirrors src/fs_paths::worktree_root — tests inherit WORKTREE_ROOT from
    // the parent shell (just test / cargo test); fail loud if unset.
    let root = std::env::var_os("WORKTREE_ROOT")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .expect("WORKTREE_ROOT unset; tests inherit it from the shell — set it in ~/.zshenv.local");
    root.join(key)
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

fn assert_ok(out: &std::process::Output, ctx: &str) {
    if out.status.success() { return; }
    let sep = if ctx.is_empty() { "" } else { "\n" };
    panic!("{ctx}{sep}stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr));
}

// Commits *all* pending changes (`add -A`) — fixtures and slot tests write a
// single target file in an otherwise-clean tree; tests with stray uncommitted
// state would silently capture it. Callers do NOT need to `git add` first.
fn git_commit(cwd: &Path, msg: &str) {
    let st = StdCommand::new("git").current_dir(cwd).args(["add", "-A"]).status().unwrap();
    assert!(st.success(), "git add -A failed in {}", cwd.display());
    let st = StdCommand::new("git")
        .current_dir(cwd)
        .args(["commit", "--quiet", "-m", msg])
        .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
        .status().unwrap();
    assert!(st.success(), "git commit -m {msg:?} failed in {}", cwd.display());
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
    std::fs::write(staging.join("README"), b"hi").unwrap();
    git_commit(&staging, "initial");
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

fn acquire_dev_sub(key: &str, name: &str) -> std::process::Output {
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", key, "acquire", "--name", name, "--group", "ios"])
        .env("GIT_ALLOW_PROTOCOL", "file")
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
    assert_ok(&out, "");
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
    assert_ok(&out, "");
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
    assert_ok(&out, "");
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
    assert_ok(&out, "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("reclaiming stale init mutex"),
        "expected stale-reclaim warning; got: {stderr}"
    );
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(path.ends_with("/post-reclaim"));
}

/// Stale pool-wide mutex (planted with old mtime) must be reclaimed by the next
/// acquire. Without auto-recovery a SIGKILL'd holder wedges every consumer of
/// the pool until manual `rm <pool>/.meta/pool.lock`. The threshold is well above
/// legitimate hold time so a real wedge surfaces inside a single retry loop.
#[test]
fn stale_pool_mutex_reclaimed_by_acquire() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // Plant a stale pool mutex (mtime = 1 year ago).
    let pool_lock = pool_root(&key).join(".meta/pool.lock");
    std::fs::create_dir_all(pool_lock.parent().unwrap()).unwrap();
    std::fs::write(&pool_lock, b"").unwrap();
    let one_year_ago = SystemTime::now() - std::time::Duration::from_secs(365 * 86400);
    std::fs::File::open(&pool_lock)
        .unwrap()
        .set_modified(one_year_ago)
        .unwrap();

    // Acquire should reclaim the stale pool mutex and proceed.
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "after-wedge", "--group", "ios"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "acquire should reclaim stale pool mutex; got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("reclaiming stale pool mutex"),
        "expected pool-mutex-reclaim warning; got: {stderr}"
    );
}

/// Pool mutex with a dead-holder PID must be reclaimed immediately, even when
/// the file's mtime is fresh (well below the 120s mtime-stale threshold). This
/// is the cmd+W / SIGHUP / panic=abort case: holder process terminates without
/// running Drop, leaks pool.lock, and the next acquire would otherwise eat the
/// full 60s busy-wait before bailing.
#[test]
fn dead_holder_pid_in_pool_mutex_reclaimed_immediately() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // Spawn-and-reap to obtain a definitely-dead PID.
    let dead_pid = {
        let mut child = StdCommand::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    };

    // Plant pool.lock with the dead PID and FRESH mtime — mtime-based stale
    // recovery would NOT fire here (file is seconds old, threshold is 120s).
    // Only PID liveness can recover this fast.
    let pool_lock = pool_root(&key).join(".meta/pool.lock");
    std::fs::create_dir_all(pool_lock.parent().unwrap()).unwrap();
    std::fs::write(&pool_lock, format!("{dead_pid}\n")).unwrap();

    let start = std::time::Instant::now();
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "after-dead-holder", "--group", "ios"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        out.status.success(),
        "acquire should reclaim dead-holder pool mutex; got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Sanity: we must NOT have eaten the 60s busy-wait. Allow generous slack
    // for cargo-test cold start; the win is "seconds, not minutes".
    assert!(
        elapsed.as_secs() < 15,
        "expected fast reclaim (<15s), took {}s",
        elapsed.as_secs()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no longer running") || stderr.contains("dead holder"),
        "expected dead-holder reclaim warning; got: {stderr}"
    );
}

/// `unstick --pool-mutex` force-clears the pool-wide mutex without waiting for
/// the auto-reclaim threshold. The flag is intended for impatient operators —
/// the test just confirms it removes the file when the flag is present and
/// reports status (without removing) when not.
#[test]
fn unstick_pool_mutex_force_clear() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let pool_lock = pool_root(&key).join(".meta/pool.lock");
    std::fs::create_dir_all(pool_lock.parent().unwrap()).unwrap();
    std::fs::write(&pool_lock, b"").unwrap(); // fresh — would NOT auto-reclaim

    // Without --pool-mutex: should report status and leave file in place.
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "unstick"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("pool mutex held"), "expected status line; got: {stdout}");
    assert!(pool_lock.exists(), "pool mutex should NOT be removed without --pool-mutex");

    // With --pool-mutex: file removed.
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "unstick", "--pool-mutex"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("force-cleared pool mutex"), "expected force-clear msg; got: {stdout}");
    assert!(!pool_lock.exists(), "pool mutex should be removed with --pool-mutex");
}

/// Two parallel acquires with --unique-sha racing on the same SHA: at most one
/// should succeed. (Currently the same-SHA scan is not pool-globally serialized,
/// so this is best-effort; see TODO.md.)
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

// ---------- submodule rename regression ----------

/// Make a bare repo whose tip commit registers a submodule. Returns the parent bare.
///
/// Layout:
///   <dir>/sub.git           (bare submodule source, one commit with "sub-content")
///   <dir>/source.git        (bare parent source, one commit with .gitmodules + sub/)
fn make_fixture_with_submodule(dir: &Path) -> PathBuf {
    // Submodule source.
    let sub_bare = dir.join("sub.git");
    run_git_root(&["init", "--quiet", "--bare", &sub_bare.display().to_string()]);
    let sub_staging = dir.join("sub-staging");
    run_git_root(&[
        "clone",
        "--quiet",
        &sub_bare.display().to_string(),
        &sub_staging.display().to_string(),
    ]);
    std::fs::write(sub_staging.join("FILE"), b"sub-content").unwrap();
    git_commit(&sub_staging, "sub initial");
    run_git(&sub_staging, &["push", "--quiet", "-u", "origin", "main"]);

    // Parent source.
    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone",
        "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    run_git(&staging, &["add", "README"]);
    // Allow `file://` submodule URLs (git 2.38+ disables by default).
    run_git(
        &staging,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            &sub_bare.display().to_string(),
            "sub",
        ],
    );
    git_commit(&staging, "with submodule");
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

/// Regression: `worktree-pool acquire` performs a directory rename
/// (`<canonical>` → `<user-name>`) before submodule init, and `release` performs
/// the inverse rename AFTER submodules have been initialized. Both renames must
/// rewrite the submodule admin's `core.worktree` pointer in
/// `<source>/.git/worktrees/<id>/modules/<name>/config`, which `git worktree
/// repair` does NOT do. Without that rewrite, the next git command in the
/// renamed slot fails with `cannot chdir to '../../<old-name>/<sub>'`.
///
/// This test exercises both:
///   acquire #1 (fresh): rename canonical→name happens BEFORE submodule init
///                       (no broken pointers yet — but verifies submodules work post-rename).
///   release #1: rename name→canonical AFTER submodule init (the path that
///               originally caused `working trees containing submodules cannot be moved`).
///   acquire #2 (recycled): rename canonical→name AFTER submodules already
///                          initialized in a previous lifecycle (the path that
///                          requires the modules/<NAME>/config rewrite).
#[test]
fn acquire_release_with_submodule_rewires_pointers() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());
    init_pool(&key, &bare);

    // ---- acquire #1: fresh slot, submodules initialized after rename ----
    let out = acquire_dev_sub(&key, "feat-1");
    assert!(
        out.status.success(),
        "acquire #1 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let slot = pool_root(&key).join("feat-1");
    // After acquire, the submodule should be checked out.
    let sub_file = slot.join("sub/FILE");
    assert!(
        sub_file.exists(),
        "submodule content missing post-acquire: {}",
        sub_file.display()
    );
    // `git status` must succeed — fails if core.worktree is wrong.
    let st = StdCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&slot)
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "git status in slot failed (stale submodule core.worktree?): {}",
        String::from_utf8_lossy(&st.stderr)
    );

    // ---- release #1: rename feat-1 → ios-0 with submodules already populated ----
    release(&key, "feat-1");
    let canonical = pool_root(&key).join("ios-0");
    assert!(canonical.exists(), "ios-0 missing post-release");
    // Pointers must be intact: a git command in the canonical slot must succeed.
    let st = StdCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&canonical)
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "git status in canonical slot failed post-release: {}",
        String::from_utf8_lossy(&st.stderr)
    );

    // ---- acquire #2: recycled slot, rename ios-0 → feat-2 with submodules already there ----
    let out = acquire_dev_sub(&key, "feat-2");
    assert!(
        out.status.success(),
        "acquire #2 (recycled) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let slot2 = pool_root(&key).join("feat-2");
    let st = StdCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&slot2)
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "git status in recycled slot failed: {}",
        String::from_utf8_lossy(&st.stderr)
    );
    // Submodule content still there from the recycled slot's prior life.
    assert!(slot2.join("sub/FILE").exists());
}

/// Acquire branches each submodule as `<slot-name>` (matches the parent slot's
/// branch). This gives commits in the submodule a push-ready label and a stable
/// ref for `wt land` to fetch by. Release un-creates it.
///
/// See CLAUDE.md §Lifecycle invariants step 11 + §Land flow.
#[test]
fn acquire_branches_submodule_release_cleans_up() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());
    init_pool(&key, &bare);

    let slot_name = "feat-branch";
    let out = acquire_dev_sub(&key, slot_name);
    assert!(
        out.status.success(),
        "acquire failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let slot = pool_root(&key).join(slot_name);
    let sub = slot.join("sub");

    // HEAD attached to refs/heads/<slot_name> (not detached).
    let head = StdCommand::new("git")
        .args(["symbolic-ref", "HEAD"])
        .current_dir(&sub)
        .output()
        .unwrap();
    assert!(head.status.success(), "submodule HEAD is detached, expected branch");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        format!("refs/heads/{slot_name}"),
    );

    // Branch ref resolves at HEAD.
    let head_sha = StdCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&sub)
        .output()
        .unwrap();
    let branch_sha = StdCommand::new("git")
        .args(["rev-parse", &format!("refs/heads/{slot_name}")])
        .current_dir(&sub)
        .output()
        .unwrap();
    assert!(branch_sha.status.success(), "submodule branch ref missing");
    assert_eq!(head_sha.stdout, branch_sha.stdout);

    // Release un-creates the branch.
    release(&key, slot_name);
    let canonical_sub = pool_root(&key).join("ios-0").join("sub");
    let after = StdCommand::new("git")
        .args(["rev-parse", "--verify", &format!("refs/heads/{slot_name}")])
        .current_dir(&canonical_sub)
        .output()
        .unwrap();
    assert!(
        !after.status.success(),
        "submodule branch '{slot_name}' should be deleted on release; rev-parse returned: {}",
        String::from_utf8_lossy(&after.stdout)
    );
}

/// Make a bare repo whose tip commit registers `n` independent submodules.
/// Each submodule is a tiny standalone bare. Returns the parent bare.
fn make_fixture_with_n_submodules(dir: &Path, n: usize) -> PathBuf {
    // Build N tiny submodule bares.
    let mut sub_bares = Vec::new();
    for i in 0..n {
        let sub_bare = dir.join(format!("sub{i}.git"));
        run_git_root(&["init", "--quiet", "--bare", &sub_bare.display().to_string()]);
        let sub_staging = dir.join(format!("sub{i}-staging"));
        run_git_root(&[
            "clone",
            "--quiet",
            &sub_bare.display().to_string(),
            &sub_staging.display().to_string(),
        ]);
        std::fs::write(sub_staging.join("FILE"), format!("sub{i}-content").as_bytes()).unwrap();
        git_commit(&sub_staging, "init");
        run_git(&sub_staging, &["push", "--quiet", "-u", "origin", "main"]);
        sub_bares.push(sub_bare);
    }

    // Parent source with N submodules.
    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone",
        "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    run_git(&staging, &["add", "README"]);
    for (i, sub_bare) in sub_bares.iter().enumerate() {
        run_git(
            &staging,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                &sub_bare.display().to_string(),
                &format!("sub{i}"),
            ],
        );
    }
    git_commit(&staging, "with submodules");
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

/// Regression for `worktree_rename`'s pre-flight clobber check (commit d175279).
///
/// On macOS, `fs::rename(from, to)` will silently replace `to` if it's an empty
/// directory (and even non-empty dirs in some configurations). This is a real
/// hazard if a prior crashed acquire/release left a stale dir at the user-name
/// path. The pre-flight `to.try_exists()` check turns the silent clobber into
/// a loud error.
///
/// Test: acquire under "feat-1", manually plant a directory at "feat-2", then
/// try to rename via a release/re-acquire cycle that would target "feat-2".
/// The simplest exposure: pre-create the target dir before an acquire — the
/// pool's lock-pick + rename will hit the bail path.
#[test]
fn acquire_refuses_when_target_dir_already_exists() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // Plant a stale dir at the slot path the next acquire will pick.
    let stale = pool_root(&key).join("planted");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("OLD_FILE"), b"prior-crash-leftover").unwrap();

    // Acquire under that name should refuse rather than clobber.
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "planted", "--group", "ios"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "acquire should refuse to clobber stale target");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already exists") || stderr.contains("refusing to clobber"),
        "expected clobber-refusal error; got: {stderr}"
    );
    // The stale file should still be there (not silently removed).
    assert!(
        stale.join("OLD_FILE").exists(),
        "stale file was clobbered: {}",
        stale.display()
    );
}

/// Regression for the self-heal path in `rewrite_slot_segment` (commit b349ec4).
///
/// Simulates the silent-corruption scenario from TODO.md: a previous
/// `worktree_rename` failed mid-walk and left a submodule's `core.worktree`
/// stuck at an old slot name (`stale-name`) — neither the source nor the
/// target of any subsequent rename. The new pool-key-anchored rewrite must
/// normalize the segment unconditionally so the next rename converges.
///
/// Steps: acquire (which clones the submodule), then **manually plant a stale
/// `core.worktree` value** in `<source>/.git/worktrees/<id>/modules/sub/config`
/// to simulate prior partial-failure state. Then release. Verify the planted
/// stale segment got normalized to the canonical slot name (not silently
/// preserved as it would have been pre-fix).
#[test]
fn release_self_heals_stale_submodule_core_worktree() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());
    init_pool(&key, &bare);

    // Acquire under user-name "feat-1" (slot home will be ios-0).
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "feat-1", "--group", "ios"])
        .output()
        .unwrap();
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-1");

    // Find the submodule's admin config inside source's worktrees admin.
    // gitlink at <slot>/.git is one line: `gitdir: <abs-path>/<id>`.
    let gitlink = std::fs::read_to_string(slot.join(".git")).unwrap();
    let gitdir = gitlink
        .strip_prefix("gitdir: ")
        .unwrap()
        .trim()
        .to_string();
    let sub_config = PathBuf::from(&gitdir).join("modules/sub/config");
    let original = std::fs::read_to_string(&sub_config).unwrap();
    assert!(
        original.contains("/feat-1/sub"),
        "expected `feat-1/sub` in {}: {original}",
        sub_config.display()
    );

    // Plant a stale segment — simulate prior partial-rewrite leftover.
    let corrupted = original.replace("/feat-1/sub", "/stale-name/sub");
    std::fs::write(&sub_config, &corrupted).unwrap();

    // Now release. The rename feat-1 → ios-0 should self-heal the stale segment
    // (anchored on pool-key, sets segment to "ios-0" regardless of current value).
    release(&key, "feat-1");

    let after = std::fs::read_to_string(&sub_config).unwrap();
    assert!(
        after.contains("/ios-0/sub"),
        "stale `/stale-name/sub` was NOT self-healed to `/ios-0/sub` after release: {after}"
    );
    assert!(
        !after.contains("/stale-name/sub"),
        "stale segment still present after release: {after}"
    );
}

/// Regression for the parallel submodule init path (`std::thread::scope` over N
/// per-submodule git processes in `submodules::update`). With N siblings competing
/// for the parent's `.git/config` lock, the per-submodule URL rewrite must still
/// land correctly for each submodule. Verifies all N submodules are present and
/// their content matches expectations after a fresh acquire.
#[test]
fn parallel_submodule_init_acquires_all() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    const N: usize = 6;
    let bare = make_fixture_with_n_submodules(tmp.path(), N);
    init_pool(&key, &bare);

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "many", "--group", "ios"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "acquire failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let slot = pool_root(&key).join("many");

    // All N submodules materialized with the right content.
    for i in 0..N {
        let file = slot.join(format!("sub{i}/FILE"));
        assert!(file.exists(), "missing submodule sub{i}: {}", file.display());
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, format!("sub{i}-content"));
    }

    // Sanity: git status in slot succeeds (all submodule core.worktree pointers ok).
    let st = StdCommand::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&slot)
        .output()
        .unwrap();
    assert!(
        st.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&st.stderr)
    );
}

// ---------- wt (bash wrapper) ----------

fn session_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin/wt")
}

/// Build a `bash <wt> --pool <key> <verb-args...>` command with PATH augmented
/// so the script can resolve `worktree-pool` to the test binary, plus a no-op
/// SHELL so an unexpected `cmd_go` launch succeeds quietly. That lets cmd_go
/// tests rely on cmd_go's *own* exit code as the signal — without it,
/// /usr/bin/zsh would fire and muddy the stderr assertions.
fn session_cmd(key: &str, args: &[&str]) -> StdCommand {
    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_worktree-pool"));
    let bin_dir = bin_path.parent().unwrap();
    let prev_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.display(), prev_path);
    let mut cmd = StdCommand::new("bash");
    cmd.arg(session_script())
        .args(["--pool", key])
        .args(args)
        .env("PATH", new_path);
    cmd.env("SHELL", "/usr/bin/true");
    // Tests run without a TTY; bypass cmd_go's TTY guard so we exercise the
    // launcher-misfire and ghost-slot paths it protects.
    cmd.env("WT_GO_ALLOW_NOTTY", "1");
    cmd
}

/// Acquire a slot, then break it. `gitlink_only=true` mirrors the most common
/// ghost-dir state (the gitlink file is gone, source-repo admin still around);
/// `false` is the harder case where the source-repo admin was also pruned —
/// matters because git's upward-walk on a missing `.git` could otherwise let
/// `slot_repo_ok` falsely pass (e.g. dotfiles repo at $HOME).
fn acquire_then_break(key: &str, name: &str, gitlink_only: bool) -> PathBuf {
    let out = acquire_dev(key, name);
    assert_ok(&out, "acquire failed");
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    assert!(path.exists());
    let gitlink = std::fs::read_to_string(path.join(".git")).expect("read gitlink");
    let admin = PathBuf::from(gitlink.strip_prefix("gitdir: ").unwrap().trim());
    std::fs::remove_file(path.join(".git")).expect("remove slot .git gitlink");
    if !gitlink_only {
        std::fs::remove_dir_all(&admin).expect("remove source-repo admin");
    }
    path
}

#[test]
fn session_go_warns_on_fast_launcher_exit() {
    // Pins two regressions in one shot — both produce the same observable
    // "banner → silent recycle" trace, distinguished only by whether the
    // launcher ran:
    //
    //   1. call_hook with `declare -F fn && fn …` propagated the missing-
    //      function non-zero status through cmd_go's `set -e`, aborting
    //      BEFORE the launcher subshell. Pools without a .wt-hooks.sh hit
    //      this every time. The misfire warning fires AFTER the subshell
    //      returns — its presence proves the subshell was reached.
    //   2. A misfired real launcher (auth/env/PATH) exits in 0s and the
    //      EXIT trap recycles silently. The warning gives the operator a
    //      breadcrumb instead of a blank "🟢 recycled cleanly".
    //
    // session_cmd sets SHELL=/usr/bin/true; the bare fixture has no hooks.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = session_cmd(&key, &["go", "fast-exit"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stderr.contains("misfired"),
        "expected fast-exit diagnostic; stdout={stdout}\nstderr={stderr}",
    );
}

#[test]
fn session_go_refuses_broken_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let path = acquire_then_break(&key, "ghost", true);

    let out = session_cmd(&key, &["go", "ghost"]).output().unwrap();
    assert!(
        !out.status.success(),
        "expected `go` to refuse broken slot; stdout={}, stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is broken (no valid .git"),
        "expected die message; got: {stderr}");
    assert!(path.exists(), "go should not delete the broken slot");
}

#[test]
fn session_cleanup_does_not_lie_on_broken_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let path = acquire_then_break(&key, "ghost", true);

    let out = session_cmd(&key, &["cleanup", "ghost"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Exit-trap contract: cleanup always exits 0 (per CLAUDE.md), so don't
    // assert non-zero. But the OUTPUT must not lie about success.
    assert!(!stdout.contains("recycled cleanly"),
        "cleanup must not falsely claim recycle on broken slot.\nstdout={stdout}\nstderr={stderr}");
    assert!(stderr.contains("🔴 BROKEN:"),
        "cleanup must surface the broken state with the documented marker.\nstderr={stderr}");
    assert!(path.exists(), "cleanup should not delete the broken slot dir");
}

/// Stress the `slot_repo_ok` predicate against the upward-walk failure mode:
/// no gitlink AND no source-repo admin. `git -C <slot> rev-parse --git-dir`
/// alone would walk up from the slot path; the explicit `[ -e <slot>/.git ]`
/// prefix is what makes the predicate correct here.
#[test]
fn session_cleanup_detects_fully_orphaned_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let path = acquire_then_break(&key, "ghost", false);

    let out = session_cmd(&key, &["cleanup", "ghost"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains("recycled cleanly"),
        "stdout={stdout}\nstderr={stderr}");
    assert!(stderr.contains("🔴 BROKEN:"),
        "stderr={stderr}");
    assert!(path.exists());
}

#[test]
fn session_rm_refuses_broken_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let path = acquire_then_break(&key, "ghost", true);

    let out = session_cmd(&key, &["rm", "ghost"]).output().unwrap();
    assert!(!out.status.success(),
        "rm should refuse broken slot rather than dive into release.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is broken (no valid .git"),
        "expected guarded refusal; got: {stderr}");
    assert!(path.exists());
}

/// Acquire a slot and dirty its working tree (tracked + untracked changes).
/// Returns the slot path.
fn acquire_then_dirty(key: &str, name: &str) -> PathBuf {
    let out = acquire_dev(key, name);
    assert_ok(&out, "acquire failed");
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    std::fs::write(path.join("README"), b"dirty tracked\n").unwrap();
    std::fs::write(path.join("untracked.txt"), b"new\n").unwrap();
    path
}

#[test]
fn session_rm_refuses_dirty_without_force() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let path = acquire_then_dirty(&key, "messy");

    let out = session_cmd(&key, &["rm", "messy"]).output().unwrap();
    assert!(!out.status.success(),
        "rm should refuse dirty slot without --force.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("dirty"), "stderr should mention dirty: {stderr}");
    assert!(path.exists());
}

#[test]
fn session_rm_force_discards_dirty() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let path = acquire_then_dirty(&key, "messy");

    let out = session_cmd(&key, &["rm", "messy", "--force"]).output().unwrap();
    assert_ok(&out, "rm --force should succeed on dirty slot");
    // The slot is un-renamed back to canonical id (ios-N) — operator namespace cleared.
    assert!(!path.exists(), "operator-named slot dir should be gone after rm --force");
}

#[test]
fn session_rm_force_discards_unmerged_branch() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "ahead");
    assert!(out.status.success());
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    // Create a commit on the slot's branch — not in main.
    std::fs::write(path.join("CHANGE"), b"new\n").unwrap();
    git_commit(&path, "ahead of main");

    // Without --force: refuse.
    let out = session_cmd(&key, &["rm", "ahead"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not in main"), "got: {stderr}");

    // With --force: succeed.
    let out = session_cmd(&key, &["rm", "ahead", "--force"]).output().unwrap();
    assert_ok(&out, "rm --force should discard unmerged commits");
    assert!(!path.exists());
}

#[test]
fn session_rm_force_accepted_before_positionals() {
    // Pin the parser contract: `rm --force <name>` works as well as
    // `rm <name> --force`. Operators reach for either order.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let path = acquire_then_dirty(&key, "messy");
    let out = session_cmd(&key, &["rm", "--force", "messy"]).output().unwrap();
    assert_ok(&out, "rm --force <name> should succeed");
    assert!(!path.exists());
}

#[test]
fn session_rm_force_still_refuses_broken_slot() {
    // --force means "discard work I know is junk" — it does NOT mean
    // "rm -rf a path that can't be released through git". Broken slots
    // need explicit `rm -rf` (no git state to release).
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let path = acquire_then_break(&key, "ghost", true);

    let out = session_cmd(&key, &["rm", "ghost", "--force"]).output().unwrap();
    assert!(!out.status.success(),
        "rm --force must still refuse broken slot.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is broken (no valid .git"), "got: {stderr}");
    assert!(path.exists());
}

/// Build a `bash <wt> <verb-args>` command rooted at `cwd`, used for verbs that
/// auto-resolve the pool key from cwd (e.g. `wt land`, which takes no `--pool`).
fn session_cmd_cwd(cwd: &Path, args: &[&str]) -> StdCommand {
    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_worktree-pool"));
    let bin_dir = bin_path.parent().unwrap();
    let prev_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.display(), prev_path);
    let mut cmd = StdCommand::new("bash");
    cmd.arg(session_script())
        .args(args)
        .current_dir(cwd)
        .env("PATH", new_path)
        .env("SHELL", "/usr/bin/true");
    cmd
}

#[test]
fn session_land_preserves_untracked_in_main_on_collision() {
    // Regression: `reset --hard` silently clobbered untracked main scratch
    // at colliding paths; `merge --ff-only` refuses instead.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // `wt land` requires main to be checked out as a worktree of source. The
    // bare fixture has none by default, so wire one alongside.
    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);

    let out = acquire_dev(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    // Slot adds tracked COLLIDE.
    std::fs::write(slot_path.join("COLLIDE"), b"slot version\n").unwrap();
    git_commit(&slot_path, "add collide");

    // Operator scratches at the same path in main worktree (untracked).
    let scratch = b"operator scratch -- must not be lost\n";
    std::fs::write(main_path.join("COLLIDE"), scratch).unwrap();

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Land must refuse loudly (not silently advance main + clobber).
    assert!(!out.status.success(),
        "land should refuse on untracked-file collision in main; \
         stdout={stdout}\nstderr={stderr}");

    // Untracked file is the load-bearing assertion: refusal w/o preservation
    // would still be a regression.
    let preserved = std::fs::read(main_path.join("COLLIDE"))
        .expect("untracked file deleted by land");
    assert_eq!(preserved, scratch,
        "untracked file in main was overwritten; stdout={stdout}\nstderr={stderr}");
}

#[test]
fn session_land_advances_main_on_clean_path() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);

    let out = acquire_dev(&key, "feat");
    assert!(out.status.success());
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    std::fs::write(slot_path.join("NEW"), b"slot-added\n").unwrap();
    git_commit(&slot_path, "add new");

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land should succeed");

    // Tree-landed in main implies ref advanced (ff-only is atomic).
    let landed = std::fs::read(main_path.join("NEW")).expect("NEW missing in main");
    assert_eq!(landed, b"slot-added\n");
}

#[test]
fn session_land_is_idempotent_on_rerun() {
    // Running land twice in a row: second run hits the `main_before == slot_head`
    // skip and exits 0 (the "resume after manual conflict resolution" contract).
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);

    let out = acquire_dev(&key, "feat");
    assert!(out.status.success());
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    std::fs::write(slot_path.join("F"), b"f\n").unwrap();
    git_commit(&slot_path, "f");

    assert!(session_cmd_cwd(&slot_path, &["land"]).output().unwrap().status.success());
    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "second land should no-op cleanly");
}

#[test]
fn session_land_keeps_first_parent_on_mainline_after_parallel_land() {
    // Two slots branched from the same main both land. The second land forces a
    // real 3-way merge; the merge commit's parents must be (current_main, slot_tip)
    // in that order — so `git log --first-parent main` stays on mainline instead
    // of diving into the second slot's history.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);

    let rev_parse = |cwd: &Path, rev: &str|
        run_git_capture(cwd, &["rev-parse", rev]).trim().to_string();

    let out_a = acquire_dev(&key, "slot-a");
    assert_ok(&out_a, "acquire slot-a");
    let slot_a = PathBuf::from(String::from_utf8_lossy(&out_a.stdout).trim().to_string());
    let out_b = acquire_dev(&key, "slot-b");
    assert_ok(&out_b, "acquire slot-b");
    let slot_b = PathBuf::from(String::from_utf8_lossy(&out_b.stdout).trim().to_string());

    std::fs::write(slot_a.join("A"), b"a\n").unwrap();
    git_commit(&slot_a, "A");
    std::fs::write(slot_b.join("B"), b"b\n").unwrap();
    git_commit(&slot_b, "B");
    let slot_b_tip = rev_parse(&slot_b, "HEAD");

    let out = session_cmd_cwd(&slot_a, &["land"]).output().unwrap();
    assert_ok(&out, "slot-a land");
    let main_after_a = rev_parse(&main_path, "HEAD");

    let out = session_cmd_cwd(&slot_b, &["land"]).output().unwrap();
    assert_ok(&out, "slot-b land");

    let parent1 = rev_parse(&main_path, "HEAD^1");
    let parent2 = rev_parse(&main_path, "HEAD^2");
    assert_eq!(parent1, main_after_a,
        "main^1 should be the prior main tip (slot-a's landed work)");
    assert_eq!(parent2, slot_b_tip,
        "main^2 should be slot-b's pre-merge tip");

    let log_text = run_git_capture(&main_path, &["log", "--first-parent", "--format=%H"]);
    assert!(!log_text.contains(&slot_b_tip),
        "first-parent log should not dive into slot-b history:\n{log_text}");

    let msg_text = run_git_capture(&main_path, &["log", "-1", "--format=%s"]);
    assert!(msg_text.contains("Merge slot-b into main"),
        "merge commit message wrong: {msg_text}");
}

#[test]
fn session_land_noop_when_already_landed() {
    // HEAD == main && clean tree && no untracked + no message → exit 0 silently
    // without acquiring land.lock or running marker / dirty / preflight scans.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);
    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);

    let out = acquire_dev(&key, "feat");
    assert_ok(&out, "acquire");
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    // Slot is at main with clean tree. Land should no-op.
    let out = session_cmd_cwd(&slot, &["land"]).output().unwrap();
    assert_ok(&out, "no-op land");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("already landed"),
        "expected 'already landed' early-exit message, got: {stdout}");
}

#[test]
fn session_land_refuses_when_main_submodule_has_in_progress_merge() {
    // Marker scan must walk top-level submodules, not just main_path. A MERGE_HEAD
    // inside <main>/<sub> blocks step-8's ff; the scan should refuse with a
    // recovery hint *before* any state change.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    run_git(&source, &["config", "submodule.sub.ignore", "untracked"]);
    init_pool(&key, &source);
    let main_sub = source.join("sub");

    // Induce MERGE_HEAD in <main>/<sub>.
    std::fs::write(main_sub.join("X"), b"v1\n").unwrap();
    git_commit(&main_sub, "x v1");
    run_git(&main_sub, &["checkout", "-b", "side"]);
    std::fs::write(main_sub.join("X"), b"side\n").unwrap();
    git_commit(&main_sub, "x side");
    run_git(&main_sub, &["checkout", "main"]);
    std::fs::write(main_sub.join("X"), b"main\n").unwrap();
    git_commit(&main_sub, "x main");
    let _ = StdCommand::new("git").current_dir(&main_sub)
        .args(["merge", "--no-edit", "side"]).output().unwrap();

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot = pool_root(&key).join("feat");
    std::fs::write(slot.join("Y"), b"y\n").unwrap();
    git_commit(&slot, "y");

    let out = session_cmd_cwd(&slot, &["land"]).output().unwrap();
    assert!(!out.status.success(), "land should refuse with MERGE_HEAD in <main>/<sub>");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("in-progress MERGE_HEAD") && stderr.contains("/sub")
            && stderr.contains("merge --abort"),
        "stderr should name the submodule path + recovery hint, got: {stderr}");
}

#[test]
fn session_land_refuses_when_main_has_in_progress_merge() {
    // Marker scan: an unfinished merge in main_path must be detected pre-flight
    // with a precise recovery hint (NOT a generic mid-flow failure).
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);
    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);

    // Create conflicting branches in main_path → induce a merge with MERGE_HEAD
    // left behind.
    std::fs::write(main_path.join("X"), b"v1\n").unwrap();
    git_commit(&main_path, "x v1");
    run_git(&main_path, &["checkout", "-b", "side"]);
    std::fs::write(main_path.join("X"), b"side\n").unwrap();
    git_commit(&main_path, "x side");
    run_git(&main_path, &["checkout", "main"]);
    std::fs::write(main_path.join("X"), b"main\n").unwrap();
    git_commit(&main_path, "x main");
    // This merge will conflict, leaving MERGE_HEAD.
    let _ = StdCommand::new("git").current_dir(&main_path)
        .args(["merge", "--no-edit", "side"]).output().unwrap();
    assert!(main_path.join(".git/MERGE_HEAD").exists()
        || run_git_capture(&main_path, &["rev-parse", "--git-path", "MERGE_HEAD"])
              .lines().next().map_or(false, |p| std::path::Path::new(p).exists()),
        "fixture failed to leave MERGE_HEAD");

    let out = acquire_dev(&key, "feat");
    assert_ok(&out, "acquire");
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    std::fs::write(slot.join("Y"), b"y\n").unwrap();
    git_commit(&slot, "y");

    let out = session_cmd_cwd(&slot, &["land"]).output().unwrap();
    assert!(!out.status.success(), "land should refuse with MERGE_HEAD in main");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("in-progress MERGE_HEAD") && stderr.contains("merge --abort"),
        "stderr should name the marker + recovery hint, got: {stderr}");
}

#[test]
fn session_land_serializes_parallel_lands_on_same_source() {
    // Two parallel `wt land` invocations on the same source must serialize via
    // land.lock: both succeed (one waits, then runs), final main contains both
    // landings. Race without the lock would surface as a noisy mid-flow failure.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);
    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);

    let out = acquire_dev(&key, "slot-a");
    assert_ok(&out, "acquire a");
    let slot_a = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let out = acquire_dev(&key, "slot-b");
    assert_ok(&out, "acquire b");
    let slot_b = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    std::fs::write(slot_a.join("A"), b"a\n").unwrap();
    git_commit(&slot_a, "A");
    std::fs::write(slot_b.join("B"), b"b\n").unwrap();
    git_commit(&slot_b, "B");

    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [slot_a.clone(), slot_b.clone()]
        .into_iter()
        .map(|slot| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                session_cmd_cwd(&slot, &["land"]).output().unwrap()
            })
        })
        .collect();
    let outs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let successes = outs.iter().filter(|o| o.status.success()).count();
    assert_eq!(successes, 2,
        "both parallel lands should succeed; failures:\n{}",
        outs.iter().filter(|o| !o.status.success())
            .map(|o| format!("stdout={}\nstderr={}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)))
            .collect::<Vec<_>>().join("\n---\n"));

    // Both landings should be reachable from main.
    let log = run_git_capture(&main_path, &["log", "--format=%s"]);
    assert!(log.contains("A") && log.contains("B"),
        "main should contain both landings; got log:\n{log}");
}

#[test]
fn session_land_refreshes_slot_submodule_when_main_brought_advance() {
    // Step 10 (slot-direction refresh): slot A bumps the submodule + lands,
    // slot B (branched before A landed) does unrelated parent-side work + lands.
    // B's parent merge brings in main's new gitlink; B's submodule clone must
    // be ff'd to match (no phantom rewinds in `git status`), attached to a
    // branch (not left detached), and the ff must be journaled in reflog.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    run_git(&source, &["config", "submodule.sub.ignore", "untracked"]);
    init_pool(&key, &source);

    let out = acquire_dev_sub(&key, "slot-a");
    assert_ok(&out, "acquire slot-a");
    let slot_a = pool_root(&key).join("slot-a");
    let out = acquire_dev_sub(&key, "slot-b");
    assert_ok(&out, "acquire slot-b");
    let slot_b = pool_root(&key).join("slot-b");

    // Slot A bumps the submodule.
    std::fs::write(slot_a.join("sub").join("ABUMP"), b"a\n").unwrap();
    git_commit(&slot_a.join("sub"), "a bumps sub");
    git_commit(&slot_a, "slot-a parent bumps sub");

    // Slot B does parent-only work (no submodule changes).
    std::fs::write(slot_b.join("BFILE"), b"b\n").unwrap();
    git_commit(&slot_b, "slot-b parent only");

    let out = session_cmd_cwd(&slot_a, &["land"]).output().unwrap();
    assert_ok(&out, "slot-a land");
    let a_sub_tip = run_git_capture(&slot_a.join("sub"), &["rev-parse", "HEAD"])
        .trim().to_string();

    let out = session_cmd_cwd(&slot_b, &["land"]).output().unwrap();
    assert_ok(&out, "slot-b land");

    let slot_b_sub = slot_b.join("sub");
    let b_sub_head = run_git_capture(&slot_b_sub, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(b_sub_head, a_sub_tip,
        "slot-b's submodule HEAD should refresh to slot-a's sub tip");

    let symref = run_git_capture(&slot_b_sub, &["symbolic-ref", "HEAD"]);
    assert!(symref.starts_with("refs/heads/"),
        "slot-b's submodule HEAD should be attached (symref), got: {symref:?}");

    let status = run_git_capture(&slot_b, &["status", "--porcelain"]);
    assert!(!status.lines().any(|l| l.contains(" sub")),
        "slot-b superproject should not show phantom submodule rewind: {status:?}");

    let reflog = run_git_capture(&slot_b_sub, &["reflog"]);
    assert!(reflog.contains(&a_sub_tip[..7]),
        "slot-b's submodule reflog should journal the ff to {} — got: {reflog}",
        &a_sub_tip[..7]);
}

#[test]
fn session_land_syncs_stale_submodule_after_plain_merge() {
    // Regression: stock `git merge main` (no --recurse-submodules, the default
    // in every IDE and `git pull`) advances the recorded submodule gitlink but
    // leaves the slot's submodule working dir at the pre-merge SHA. `wt land`'s
    // auto-commit step previously ran `git add -u` blind, which staged the
    // stale working HEAD as the new gitlink — silently regressing the merge.
    // Step 11's preflight then caught its own corruption and aborted, leaving
    // a spurious commit and unadvanced main.
    //
    // Fix: detect the working-behind-index case before auto-commit and run
    // `git submodule update` to bring the working dir forward. Then auto-commit
    // sees a clean tree (or only legit changes) and skips the spurious commit.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    run_git(&source, &["config", "submodule.sub.ignore", "untracked"]);
    init_pool(&key, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = pool_root(&key).join("feat");

    // Advance main's submodule gitlink: commit in main's submodule clone,
    // then commit the gitlink advance in main's parent.
    let main_sub = source.join("sub");
    std::fs::write(main_sub.join("MAIN_BUMP"), b"main-bump\n").unwrap();
    git_commit(&main_sub, "main bumps sub");
    git_commit(&source, "main parent bumps sub");
    let main_new_gitlink = run_git_capture(&main_sub, &["rev-parse", "HEAD"])
        .trim().to_string();

    // Slot does parent-only work (no submodule changes) so the upcoming merge
    // creates a real merge commit rather than fast-forwarding.
    std::fs::write(slot_path.join("SLOT_FILE"), b"slot\n").unwrap();
    git_commit(&slot_path, "slot work");

    // Stock `git merge` — does NOT update the submodule working dir. The
    // recorded gitlink advances to main's new SHA; the slot's submodule clone
    // HEAD stays at the original.
    let st = StdCommand::new("git")
        .args(["merge", "--no-edit", "main"])
        .current_dir(&slot_path)
        .status().unwrap();
    assert!(st.success(), "git merge main in slot");

    // Pre-condition: stale working HEAD, advanced index.
    let slot_sub = slot_path.join("sub");
    let stale_head = run_git_capture(&slot_sub, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(stale_head, main_new_gitlink,
        "pre-condition: slot's sub working HEAD should still be at the pre-merge SHA");
    let recorded_after_merge = run_git_capture(&slot_path, &["ls-tree", "HEAD", "sub"])
        .split_whitespace().nth(2).unwrap_or("").to_string();
    assert_eq!(recorded_after_merge, main_new_gitlink,
        "pre-condition: slot's recorded gitlink should equal main's new gitlink");

    // wt land with NO message — without the fix this would refuse with
    // "dirty tracked work needs a commit message" (the stale-sub mismatch
    // shows up as dirty). With the fix, the sub sync clears the dirt and
    // land proceeds without an auto-commit.
    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land on stale-submodule state");

    // Main's recorded gitlink must equal main_new_gitlink (no regression).
    let main_recorded_after_land = run_git_capture(&source, &["ls-tree", "HEAD", "sub"])
        .split_whitespace().nth(2).unwrap_or("").to_string();
    assert_eq!(main_recorded_after_land, main_new_gitlink,
        "main's recorded gitlink must not regress after land");

    // No spurious "WIP via land" or similar regression auto-commit.
    let log = run_git_capture(&source, &["log", "--format=%s", "-n", "10"]);
    assert!(!log.contains("WIP via land"),
        "land must not produce a regression auto-commit; log:\n{log}");

    // Slot's sub working HEAD now matches the recorded gitlink (sub-sync ran).
    let synced_head = run_git_capture(&slot_sub, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(synced_head, main_new_gitlink,
        "slot's sub working HEAD should have been ff'd forward by the pre-stage sync");
}

#[test]
fn session_land_preserves_untracked_in_submodule_on_collision() {
    // Same untracked-clobber bug as the parent-level fix, but at the submodule
    // propagation step (`reset --hard <new_sha>` in main's submodule clone).
    // Critical for projects with many actively-edited submodules (e.g. Unity
    // Packages).
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    // Real-world setup: source is a non-bare working clone where main IS the
    // source's primary checkout (slots' per-worktree modules live separately).
    // A bare-with-extra-worktree fixture would share `<bare>/modules/sub` with
    // the slot, contaminating the test.
    make_fixture_with_submodule(tmp.path()); // builds <tmp>/source.git + <tmp>/staging
    let source = tmp.path().join("staging");
    // Real-world projects with active submodule editing typically silence the
    // "M sub" noise from untracked content inside submodules — without this,
    // land's precheck refuses before reaching the propagation block.
    run_git(&source, &["config", "submodule.sub.ignore", "untracked"]);
    init_pool(&key, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = pool_root(&key).join("feat");
    let slot_sub = slot_path.join("sub");

    // Slot adds tracked COLLIDE in submodule, commits, advances parent gitlink.
    std::fs::write(slot_sub.join("COLLIDE"), b"slot version\n").unwrap();
    git_commit(&slot_sub, "add collide in sub");
    git_commit(&slot_path, "bump sub");

    // Operator scratches at COLLIDE in main's submodule clone (untracked).
    let main_sub = source.join("sub");
    let scratch = b"operator scratch in submodule -- must not be lost\n";
    std::fs::write(main_sub.join("COLLIDE"), scratch).unwrap();

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    let preserved = std::fs::read(main_sub.join("COLLIDE"))
        .expect("untracked submodule file deleted by land");
    assert_eq!(preserved, scratch,
        "untracked submodule scratch was overwritten; \
         success={} stdout={stdout}\nstderr={stderr}", out.status.success());

    // Critical: parent main MUST NOT have advanced. If it did, re-running
    // land would see no `moved` gitlinks and skip the submodule loop —
    // failed submodules would never get retried.
    let main_after = StdCommand::new("git")
        .args(["-C", &source.display().to_string(), "rev-parse", "main"])
        .output().unwrap();
    let main_initial = StdCommand::new("git")
        .args(["-C", &source.display().to_string(), "rev-parse", "main@{1}"])
        .output();
    // Read main's current SHA and compare against the slot's parent commit
    // (which would be the post-land target if the parent had advanced).
    let slot_head = StdCommand::new("git")
        .args(["-C", &slot_path.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    assert_ne!(main_after.stdout, slot_head.stdout,
        "parent main advanced despite submodule-propagation failure — \
         operator can't recover via re-run");
    let _ = main_initial;
}

#[test]
fn session_land_recovers_after_submodule_collision_resolved() {
    // After a collision-failed land, removing the offending untracked file
    // and re-running land must complete cleanly: submodule advances + parent
    // advances. Pins the order-reversal contract: submodule work happens
    // before parent ff-merge so partial failures stay recoverable.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    run_git(&source, &["config", "submodule.sub.ignore", "untracked"]);
    init_pool(&key, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert!(out.status.success());
    let slot_path = pool_root(&key).join("feat");
    let slot_sub = slot_path.join("sub");

    std::fs::write(slot_sub.join("COLLIDE"), b"slot version\n").unwrap();
    git_commit(&slot_sub, "add collide");
    git_commit(&slot_path, "bump sub");

    let main_sub = source.join("sub");
    std::fs::write(main_sub.join("COLLIDE"), b"scratch\n").unwrap();

    // First run fails (collision).
    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert!(!out.status.success(), "first land should fail on collision");

    // Operator resolves: remove the untracked file.
    std::fs::remove_file(main_sub.join("COLLIDE")).unwrap();

    // Second run succeeds.
    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "second land should succeed after collision cleared;");

    // Sub + parent both advanced.
    let slot_sub_head = StdCommand::new("git")
        .args(["-C", &slot_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    let main_sub_head = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    assert_eq!(main_sub_head.stdout, slot_sub_head.stdout, "submodule did not advance");
    let slot_head = StdCommand::new("git")
        .args(["-C", &slot_path.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    let main_head = StdCommand::new("git")
        .args(["-C", &source.display().to_string(), "rev-parse", "main"])
        .output().unwrap();
    assert_eq!(main_head.stdout, slot_head.stdout, "parent main did not advance");
}

#[test]
fn session_land_attaches_detached_submodule_to_main() {
    // `git submodule update` leaves submodules detached; without an explicit
    // attach, ff-only would advance HEAD only and leave `main` ref lagging.
    // Verifies the detached → main attach path before the ff-merge.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    init_pool(&key, &source);

    // Simulate `git submodule update` aftermath: detach source/sub at gitlink SHA.
    let main_sub = source.join("sub");
    let head_sha = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    let sha = String::from_utf8_lossy(&head_sha.stdout).trim().to_string();
    run_git(&main_sub, &["checkout", "--quiet", "--detach", &sha]);

    let out = acquire_dev_sub(&key, "feat");
    assert!(out.status.success());
    let slot_path = pool_root(&key).join("feat");
    let slot_sub = slot_path.join("sub");

    std::fs::write(slot_sub.join("NEW"), b"x\n").unwrap();
    git_commit(&slot_sub, "x");
    git_commit(&slot_path, "bump sub");

    let slot_sub_head = StdCommand::new("git")
        .args(["-C", &slot_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land");

    // HEAD must now be on refs/heads/main (re-attached).
    let head_ref = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "symbolic-ref", "HEAD"])
        .output().unwrap();
    assert!(head_ref.status.success(),
        "main_sub HEAD is still detached after land");
    assert_eq!(String::from_utf8_lossy(&head_ref.stdout).trim(), "refs/heads/main");

    // And main ref must equal slot's submodule HEAD.
    let main_ref_sha = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "rev-parse", "refs/heads/main"])
        .output().unwrap();
    assert_eq!(main_ref_sha.stdout, slot_sub_head.stdout);
}

#[test]
fn session_land_attaches_detached_submodule_to_gitmodules_branch() {
    // `.gitmodules` branch hint takes priority over the `main` fallback. If
    // the submodule has `branch = release` declared, attach to release on
    // detached HEAD, not main.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");

    let main_sub = source.join("sub");
    let head_sha = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    let sha = String::from_utf8_lossy(&head_sha.stdout).trim().to_string();
    run_git(&main_sub, &["branch", "release", &sha]);
    run_git(&source, &["config", "-f", ".gitmodules", "submodule.sub.branch", "release"]);
    git_commit(&source, "track release branch for sub");
    run_git(&source, &["push", "--quiet", "origin", "main"]);
    run_git(&main_sub, &["checkout", "--quiet", "--detach", &sha]);

    init_pool(&key, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = pool_root(&key).join("feat");
    let slot_sub = slot_path.join("sub");

    std::fs::write(slot_sub.join("X"), b"x\n").unwrap();
    git_commit(&slot_sub, "x");
    git_commit(&slot_path, "bump sub");

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land");

    let head_ref = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "symbolic-ref", "HEAD"])
        .output().unwrap();
    assert!(head_ref.status.success(), "main_sub HEAD detached after land");
    assert_eq!(String::from_utf8_lossy(&head_ref.stdout).trim(),
               "refs/heads/release",
               "expected attach to release per .gitmodules, not main");
}

#[test]
fn session_land_propagates_submodule_to_main() {
    // Pin against `702d3ba`-era dead `[ -d $sub/.git ]` check: submodule
    // `.git` is a gitlink *file*, not a directory, so the loop body was
    // skipped on every land. Catches future regressions where propagation
    // silently no-ops while the parent advances.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    init_pool(&key, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = pool_root(&key).join("feat");
    let slot_sub = slot_path.join("sub");

    std::fs::write(slot_sub.join("NEW"), b"x\n").unwrap();
    git_commit(&slot_sub, "x");
    git_commit(&slot_path, "bump sub");

    let slot_sub_head = StdCommand::new("git")
        .args(["-C", &slot_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land");

    let main_sub = source.join("sub");
    let main_sub_head = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    assert_eq!(main_sub_head.stdout, slot_sub_head.stdout,
        "main's submodule clone was not propagated to slot's HEAD");
    assert!(main_sub.join("NEW").exists(),
        "submodule's new tracked file did not land in main worktree");
}

#[test]
fn session_land_refuses_when_main_worktree_on_other_branch() {
    // Operator detour: if main_path has a different branch checked out, land
    // must refuse — `merge --ff-only` would otherwise advance the wrong branch.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);
    // Operator detour: check out a different branch in the main worktree.
    run_git(&main_path, &["checkout", "--quiet", "-b", "operator-side"]);

    let out = acquire_dev(&key, "feat");
    assert!(out.status.success());
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    std::fs::write(slot_path.join("X"), b"x\n").unwrap();
    git_commit(&slot_path, "x");

    // `find_main_path` parses `worktree list --porcelain` for `branch refs/heads/main`;
    // after the operator's checkout, that line is gone, so land errors with
    // "main is not checked out in any worktree". Either that or the symref
    // guard would catch it — load-bearing assertion is the operator branch
    // didn't move.
    let main_before = StdCommand::new("git")
        .args(["-C", &main_path.display().to_string(), "rev-parse", "operator-side"])
        .output().unwrap();
    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert!(!out.status.success(),
        "land must refuse; stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let main_after = StdCommand::new("git")
        .args(["-C", &main_path.display().to_string(), "rev-parse", "operator-side"])
        .output().unwrap();
    assert_eq!(main_before.stdout, main_after.stdout,
        "operator-side branch must not move");
}


// ---------- crash-recovery tests (release reorder + reclaim_stale) ----------
//
// We compose each post-crash on-disk shape directly via fs ops, then verify
// the next acquire/release converges to a clean state.

fn slot_gitdir_path(slot: &Path) -> PathBuf {
    let text = std::fs::read_to_string(slot.join(".git")).unwrap();
    let rest = text.strip_prefix("gitdir: ").unwrap().trim();
    PathBuf::from(rest)
}

fn run_git_capture(cwd: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {} failed: {}",
        args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn reclaim_legacy_zombie_at_acquire() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-zombie");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-zombie");
    let gitdir = slot_gitdir_path(&slot);
    std::fs::remove_file(gitdir.join("worktree-pool/lock")).unwrap();
    run_git(&slot, &["checkout", "--quiet", "--detach"]);
    run_git(&slot, &["branch", "-D", "feat-zombie"]);
    assert!(slot.exists());
    assert!(!gitdir.join("worktree-pool/lock").exists());

    let out = acquire_dev(&key, "feat-fresh");
    assert_ok(&out, "acquire must succeed (zombie reclaimed first)");
    assert!(!slot.exists(), "zombie should have been un-renamed away from feat-zombie");
}

#[test]
fn release_replay_completes_after_slow_ops_crash() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-half");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-half");
    run_git(&slot, &["checkout", "--quiet", "--detach"]);
    run_git(&slot, &["branch", "-D", "feat-half"]);
    assert!(slot_gitdir_path(&slot).join("worktree-pool/lock").exists(),
        "lock still present (new-ordering crash invariant)");

    release(&key, "feat-half");

    assert!(!slot.exists(), "feat-half dir should be un-renamed by replay");
    let out = acquire_dev(&key, "feat-after");
    assert_ok(&out, "");
}

#[test]
fn reclaim_orphan_lock_after_post_rename_crash() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-postrename");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-postrename");
    let gitdir = slot_gitdir_path(&slot);

    release(&key, "feat-postrename");
    let canonical = pool_root(&key).join("ios-0");
    assert!(canonical.exists(), "released slot lives at ios-0");
    assert_eq!(gitdir, slot_gitdir_path(&canonical), "gitdir stable across rename");

    let lock_path = gitdir.join("worktree-pool/lock");
    std::fs::write(&lock_path,
        "started_at: 2026-01-01T00:00:00Z\nfull_sha: 0000000000000000000000000000000000000000\ngroup: ios\n",
    ).unwrap();

    // Trigger reclaim_stale via release of an unrelated name (does not write a
    // new lock to ios-0's gitdir, so we can check the orphan is actually gone).
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "release", "--name", "no-such-slot"])
        .assert()
        .success();
    assert!(!lock_path.exists(),
        "reclaim_stale must remove the orphan lock at {}", lock_path.display());

    // And the canonical ios-0 slot is now correctly classified as idle and
    // available — fresh acquire reuses it (recycled-warm path).
    let out = acquire_dev(&key, "feat-after-orphan");
    assert_ok(&out, "acquire after reclaim must succeed");
    let acquired_path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(acquired_path.ends_with("/feat-after-orphan"));
}

#[test]
fn reclaim_does_not_disturb_live_held_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "live-1");
    assert_ok(&out, "");
    let live_slot = pool_root(&key).join("live-1");
    let live_gitdir = slot_gitdir_path(&live_slot);
    let live_lock_before = std::fs::read(live_gitdir.join("worktree-pool/lock")).unwrap();

    let out = acquire_dev(&key, "live-2");
    assert_ok(&out, "");

    assert!(live_slot.exists(), "live held slot must remain at user-name");
    let live_lock_after = std::fs::read(live_gitdir.join("worktree-pool/lock")).unwrap();
    assert_eq!(live_lock_before, live_lock_after, "live lock must not be touched");
    let head = run_git_capture(&live_slot, &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(head.trim(), "live-1", "live slot's HEAD must still be on its branch");
}

#[test]
fn reclaim_multiple_legacy_zombies_in_one_sweep() {
    // Mirrors the user's actual pspec pool state: three SIGINT-induced zombies
    // present simultaneously when the next acquire/release runs. Each must be
    // reclaimed independently in the single sweep.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    for name in ["z1", "z2", "z3"] {
        let out = acquire_dev(&key, name);
        assert_ok(&out, "");
        let slot = pool_root(&key).join(name);
        let gitdir = slot_gitdir_path(&slot);
        std::fs::remove_file(gitdir.join("worktree-pool/lock")).unwrap();
        run_git(&slot, &["checkout", "--quiet", "--detach"]);
        run_git(&slot, &["branch", "-D", name]);
    }

    // One sweep (triggered by acquire) must reclaim all three.
    let out = acquire_dev(&key, "fresh");
    assert_ok(&out, "acquire must succeed with three zombies present");

    for name in ["z1", "z2", "z3"] {
        assert!(!pool_root(&key).join(name).exists(),
            "zombie '{name}' should have been un-renamed");
    }
}

/// The user's actual pspec crash was in a submodule-bearing pool — release's
/// `submodules::delete_branch_recursive` (the slowest step in the new ordering)
/// is exactly the SIGINT-prone window. Verify replay completes the submodule
/// branch deletes cleanly when the slot is half-released.
#[test]
fn release_replay_completes_with_submodules() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-sub");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-sub");

    // Submodule is initialized; acquire created a `feat-sub` branch in it.
    let sub = slot.join("sub");
    assert!(sub.join(".git").exists(), "submodule should be initialized");
    let sub_branches_before = run_git_capture(&sub, &["branch", "--list", "feat-sub"]);
    assert!(sub_branches_before.contains("feat-sub"),
        "submodule should have feat-sub branch: {sub_branches_before:?}");

    // Compose new-ordering crash: parent branch deleted, lock still present.
    run_git(&slot, &["checkout", "--quiet", "--detach"]);
    run_git(&slot, &["branch", "-D", "feat-sub"]);
    assert!(slot_gitdir_path(&slot).join("worktree-pool/lock").exists());

    // Replay completes the un-rename + submodule branch cleanup.
    release(&key, "feat-sub");
    assert!(!slot.exists(), "feat-sub should be un-renamed by replay");

    // Submodule's per-slot branch should be gone too.
    let canonical_sub = pool_root(&key).join("ios-0").join("sub");
    let sub_branches_after = run_git_capture(&canonical_sub, &["branch", "--list", "feat-sub"]);
    assert!(sub_branches_after.trim().is_empty(),
        "submodule's feat-sub branch should be cleaned: {sub_branches_after:?}");
}

/// Acquire+release each `name` once to materialize its canonical-N dir on
/// disk as an idle slot. Used by capacity tests that need `reclaim_stale`'s
/// `smallest_free_n` to find every canonical-N already occupied.
fn materialize_idle_canonicals(key: &str, group: Option<&str>, names: &[&str]) {
    for n in names {
        let mut acq = Command::cargo_bin("worktree-pool").unwrap();
        acq.args(["--pool", key, "acquire", "--name", n]);
        if let Some(g) = group {
            acq.args(["--group", g]);
        }
        acq.output().unwrap();
    }
    for n in names {
        Command::cargo_bin("worktree-pool").unwrap()
            .args(["--pool", key, "release", "--name", n])
            .assert().success();
    }
}

/// Plant an unrecoverable-zombie shape at `<pool>/<name>` via direct
/// `git worktree add --detach` plus a pool-ownership marker — simulates a
/// crashed acquire/release (lock removed but the slot was once pool-owned).
/// Without the marker, `reclaim_stale` correctly refuses to absorb the dir
/// (it'd be indistinguishable from a manual `git worktree add`).
fn plant_zombie(bare: &Path, pool: &Path, name: &str) {
    let st = StdCommand::new("git")
        .args(["-C", &bare.display().to_string(),
               "worktree", "add", "--detach",
               &pool.join(name).display().to_string(), "main"])
        .status().unwrap();
    assert!(st.success(), "plant_zombie: `git worktree add` failed for {name}");

    // Resolve the gitdir via the gitlink the worktree just created.
    let gitlink = std::fs::read_to_string(pool.join(name).join(".git")).unwrap();
    let gitdir = std::path::PathBuf::from(
        gitlink.trim().trim_start_matches("gitdir:").trim()
    );
    let marker = gitdir.join("worktree-pool/created");
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(&marker, "# planted by test\n").unwrap();
}

/// Over-provisioned state — every `0..max_slots` canonical-N is occupied as
/// idle on disk beside zombies. `reclaim_stale` relocates the zombies to
/// surplus N's (>= max_slots), then acquire proceeds. End-to-end self-heal.
#[test]
fn over_provisioned_pool_self_heals_via_reclaim() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());

    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "init"])
        .arg("--source").arg(&bare)
        .args(["--max-slots", "2"])
        .assert().success();

    let pool = pool_root(&key);
    materialize_idle_canonicals(&key, None, &["warm-1", "warm-2"]);
    assert!(pool.join("slot-0").exists(), "slot-0 idle canonical present");
    assert!(pool.join("slot-1").exists(), "slot-1 idle canonical present");

    // Plant two zombies. Pool now has 2 Renamed (zombies) + 2 Canonical
    // (idle) = 4 dirs — 2 over max_slots.
    plant_zombie(&bare, &pool, "z1");
    plant_zombie(&bare, &pool, "z2");

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "fresh-1"])
        .output().unwrap();
    assert_ok(&out, "acquire must succeed after reclaim relocates zombies to surplus N's");
    assert!(pool.join("fresh-1").exists());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("released 'z1'") && stderr.contains("released 'z2'"),
        "reclaim_stale should report relocating both zombies; stderr={stderr}");
}

/// Grouped variant of self-healing: a zombie's group is unknown, so reclaim
/// relocates it under one of the configured groups. The requested group's
/// acquire then succeeds end-to-end.
#[test]
fn over_provisioned_pool_self_heals_grouped() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());

    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "init"])
        .arg("--source").arg(&bare)
        .args(["--max-slots", "1", "--groups", "ios,android"])
        .assert().success();

    materialize_idle_canonicals(&key, Some("ios"), &["warm-ios"]);
    materialize_idle_canonicals(&key, Some("android"), &["warm-android"]);
    let pool = pool_root(&key);
    plant_zombie(&bare, &pool, "z-mystery");

    let out = Command::cargo_bin("worktree-pool").unwrap()
        .args(["--pool", &key, "acquire", "--name", "fresh-ios", "--group", "ios"])
        .output().unwrap();
    assert_ok(&out, "acquire --group ios must succeed; zombie is recoverable");
    assert!(pool.join("fresh-ios").exists());
}

/// Release lands at a surplus N when every `0..max_slots` canonical is
/// occupied as idle on disk beside a held name. Reachable when reclaim turns
/// multiple zombies into idle canonicals beside an existing held slot, or
/// when `max_slots` is reduced after slots were materialized.
#[test]
fn release_unblocks_when_all_canonicals_idle() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());

    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "init"])
        .arg("--source").arg(&bare)
        .args(["--max-slots", "2"])
        .assert().success();

    let pool = pool_root(&key);

    // Build slot-0 idle + slot-1 idle + feat held = 3 dirs. Natural
    // acquire/release can't reach this (total_dirs ≤ max_slots holds in the
    // happy path), so plant the held "feat" directly via worktree add + lock
    // write after filling both canonicals.
    materialize_idle_canonicals(&key, None, &["warm-a", "warm-b"]);
    assert!(pool.join("slot-0").exists(), "slot-0 should be idle on disk");
    assert!(pool.join("slot-1").exists(), "slot-1 should be idle on disk");

    plant_zombie(&bare, &pool, "feat");
    // Write a lock at feat's gitdir → promotes the zombie to a real held slot.
    let gitdir_out = StdCommand::new("git")
        .args(["-C", &pool.join("feat").display().to_string(),
               "rev-parse", "--git-dir"])
        .output().unwrap();
    assert!(gitdir_out.status.success(), "rev-parse --git-dir failed for feat");
    let gd = String::from_utf8(gitdir_out.stdout).unwrap().trim().to_string();
    let gd_path = if Path::new(&gd).is_absolute() {
        PathBuf::from(&gd)
    } else {
        pool.join("feat").join(&gd)
    };
    let lock_dir = gd_path.join("worktree-pool");
    std::fs::create_dir_all(&lock_dir).unwrap();
    let head_sha = String::from_utf8(
        StdCommand::new("git")
            .args(["-C", &bare.display().to_string(), "rev-parse", "HEAD"])
            .output().unwrap().stdout
    ).unwrap().trim().to_string();
    std::fs::write(
        lock_dir.join("lock"),
        format!("started_at: 2026-05-10T00:00:00Z\nfull_sha: {head_sha}\n"),
    ).unwrap();
    assert!(pool.join("feat").exists(), "feat should be held");

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "release", "--name", "feat"])
        .output().unwrap();
    assert_ok(&out, "release must succeed in over-provisioned pool");
    assert!(!pool.join("feat").exists(), "feat dir should be gone after release");
    assert!(pool.join("slot-2").exists(),
        "released slot should land at slot-2 (smallest free N past the surplus)");

    // Surplus reuse: a fresh acquire must not grow the pool — the surplus
    // slot-2 should be reused (or one of the lower idle canonicals), not
    // appended-to with a fresh slot-N. Asserts `acquirable_ns`'s surplus scan.
    Command::cargo_bin("worktree-pool").unwrap()
        .args(["--pool", &key, "acquire", "--name", "reuse"])
        .assert().success();
    let pool_dir_count = std::fs::read_dir(&pool)
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|e| {
            let n = e.file_name();
            !n.to_string_lossy().starts_with('.') && e.path().is_dir()
        })
        .count();
    assert!(pool_dir_count <= 3,
        "pool should not grow past pre-acquire size (3 dirs); got {pool_dir_count}");
}

// ---------- pool-ownership marker tests (provenance gate) ----------

/// Resolve `<gitdir>/worktree-pool/created` for a slot at `<pool>/<name>`.
fn marker_for(pool: &Path, name: &str) -> PathBuf {
    let gitlink = std::fs::read_to_string(pool.join(name).join(".git")).unwrap();
    let gitdir = PathBuf::from(gitlink.trim().trim_start_matches("gitdir:").trim());
    gitdir.join("worktree-pool/created")
}

/// `acquire` writes both lock and marker. `release` removes the lock but
/// preserves the marker — provenance is sticky once stamped.
#[test]
fn acquire_writes_marker_and_release_preserves_it() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-x");
    assert_ok(&out, "acquire feat-x");

    let pool = pool_root(&key);
    let marker = marker_for(&pool, "feat-x");
    assert!(marker.exists(), "acquire should stamp marker at {}", marker.display());

    release(&key, "feat-x");

    // Post-release the slot is back at canonical ios-0; marker must follow the gitdir.
    let canonical_marker = marker_for(&pool, "ios-0");
    assert!(canonical_marker.exists(),
        "marker must persist past release at {}", canonical_marker.display());
}

/// Manual `git worktree add` under the pool root must NOT be silently
/// absorbed by reclaim_stale. Pre-fix, this destroyed the operator's branch
/// and un-renamed the dir on the next acquire from any session.
#[test]
fn reclaim_stale_skips_unmanaged_renamed_dir() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let pool = pool_root(&key);
    // Plant a manual worktree (no marker — operator bypassed `acquire`).
    let st = StdCommand::new("git")
        .args(["-C", &bare.display().to_string(),
               "worktree", "add",
               &pool.join("manual-feat").display().to_string()])
        .status().unwrap();
    assert!(st.success(), "manual git worktree add failed");

    // Trigger reclaim_stale via an unrelated acquire.
    let out = acquire_dev(&key, "real-feat");
    assert_ok(&out, "acquire of real-feat must succeed alongside unmanaged dir");

    assert!(pool.join("manual-feat").exists(),
        "unmanaged dir must NOT be absorbed by reclaim_stale");
    assert!(pool.join("real-feat").exists(), "real acquire succeeded");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not pool-owned") && stderr.contains("manual-feat"),
        "reclaim_stale should warn about the unmanaged dir; stderr={stderr}");
}

/// Migration path: pre-fix pools have lock files but no marker. The
/// auto-stamp pass at the start of reclaim_stale should write the marker
/// for any slot whose lock proves pool ownership.
#[test]
fn reclaim_stale_auto_stamps_marker_on_legacy_locked_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "legacy");
    assert_ok(&out, "acquire legacy");

    let pool = pool_root(&key);
    let marker = marker_for(&pool, "legacy");
    assert!(marker.exists(), "precondition: marker present after acquire");

    // Simulate a pre-fix pool: hand-delete the marker, leaving the lock.
    std::fs::remove_file(&marker).unwrap();
    assert!(!marker.exists(), "marker removed");

    // Trigger reclaim_stale via an unrelated acquire.
    let out = acquire_dev(&key, "fresh");
    assert_ok(&out, "acquire fresh must trigger reclaim_stale migration");

    assert!(marker.exists(), "reclaim_stale should auto-stamp the marker on the legacy locked slot");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("stamping legacy pool marker") && stderr.contains("legacy"),
        "auto-stamp should be logged; stderr={stderr}");
}

// ---------- stale `index.lock` reclamation ----------
//
// Plant a per-worktree `index.lock` matching the crashed-git signature
// (0-byte + aged-out mtime), trigger reclaim_stale via an unrelated release,
// and verify removal. Guards: live (young) and non-empty locks must survive.

/// Plant `<gitdir>/index.lock` at `bytes.len()` bytes, with mtime backdated
/// by `age`. Returns the lock path.
fn plant_index_lock(gitdir: &Path, bytes: &[u8], age: std::time::Duration) -> PathBuf {
    let lock = gitdir.join("index.lock");
    std::fs::write(&lock, bytes).unwrap();
    let when = std::time::SystemTime::now() - age;
    std::fs::File::open(&lock).unwrap().set_modified(when).unwrap();
    lock
}

/// Trigger `reclaim_stale` without writing any new locks to held slots by
/// releasing a name that doesn't exist (release short-circuits after the
/// reclaim pass — see release.rs).
fn trigger_reclaim(key: &str) {
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", key, "release", "--name", "no-such-slot"])
        .assert()
        .success();
}

#[test]
fn reclaim_clears_stale_index_lock() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-stale");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-stale");
    let gitdir = slot_gitdir_path(&slot);

    let lock = plant_index_lock(&gitdir, b"", std::time::Duration::from_secs(120));
    assert!(lock.exists(), "precondition: planted lock present");

    trigger_reclaim(&key);

    assert!(!lock.exists(),
        "reclaim_stale must remove the stale index.lock at {}", lock.display());
}

#[test]
fn reclaim_preserves_young_index_lock() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-young");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-young");
    let gitdir = slot_gitdir_path(&slot);

    // 0-byte but fresh — could be a live `git status` mid-stride. Must survive.
    let lock = plant_index_lock(&gitdir, b"", std::time::Duration::from_secs(2));

    trigger_reclaim(&key);

    assert!(lock.exists(),
        "young 0-byte lock must NOT be removed (live git protection)");

    // Cleanup so the held slot can be released by `Cleanup`.
    std::fs::remove_file(&lock).unwrap();
}

#[test]
fn reclaim_preserves_nonempty_index_lock() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-partial");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-partial");
    let gitdir = slot_gitdir_path(&slot);

    // Aged but non-empty — could be a partial write the operator wants to see.
    let lock = plant_index_lock(&gitdir, b"pid 12345\n", std::time::Duration::from_secs(120));

    trigger_reclaim(&key);

    assert!(lock.exists(),
        "non-zero lock must NOT be removed (operator-visible partial write)");
    std::fs::remove_file(&lock).unwrap();
}

#[test]
fn doctor_reports_stale_index_lock() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-doc");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-doc");
    let gitdir = slot_gitdir_path(&slot);

    let lock = plant_index_lock(&gitdir, b"", std::time::Duration::from_secs(120));

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .arg("doctor")
        .output()
        .unwrap();
    assert_ok(&out, "doctor must exit ok even when warnings are present");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("! stale index.lock"),
        "doctor must emit the WARN marker for stale locks; stdout={stdout}");
    assert!(stdout.contains(&lock.display().to_string()),
        "doctor should report the offending path; stdout={stdout}");
    assert!(lock.exists(),
        "doctor must be read-only — stale lock should persist until acquire/release clears it");

    std::fs::remove_file(&lock).unwrap();
}

#[test]
fn reclaim_preserves_symlink_at_index_lock() {
    // `symlink_metadata` + `is_file()` guard: a symlink at `index.lock` (any
    // size) is never git's work — leave alone, regardless of age/target.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-symlink");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-symlink");
    let gitdir = slot_gitdir_path(&slot);
    let lock = gitdir.join("index.lock");

    // Target file is 0 bytes & old — would qualify if it were the lock itself.
    let target = tmp.path().join("zero");
    std::fs::write(&target, b"").unwrap();
    std::fs::File::open(&target).unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
        .unwrap();
    std::os::unix::fs::symlink(&target, &lock).unwrap();

    trigger_reclaim(&key);

    assert!(std::fs::symlink_metadata(&lock).is_ok(),
        "symlink at index.lock must NOT be removed");
    std::fs::remove_file(&lock).unwrap();
}

#[test]
fn doctor_reports_no_locks_for_clean_pool() {
    // Tests share `$WORKTREE_ROOT` with the host's other pools, so we can't
    // assert "none" globally — assert that doctor doesn't surface a stale
    // lock attributable to our pool's gitdirs.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-clean");
    assert_ok(&out, "");
    let slot = pool_root(&key).join("feat-clean");
    let our_gitdir = slot_gitdir_path(&slot);

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .arg("doctor")
        .output()
        .unwrap();
    assert_ok(&out, "");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("stale index.lock:"),
        "doctor output missing stale-lock check line: {stdout}");
    let our_prefix = our_gitdir.display().to_string();
    assert!(!stdout.contains(&our_prefix),
        "doctor falsely flagged our clean pool's gitdir ({our_prefix}); stdout={stdout}");
}
