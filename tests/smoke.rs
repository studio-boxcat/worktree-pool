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
    run_git(&sub_staging, &["config", "user.email", "t@t"]);
    run_git(&sub_staging, &["config", "user.name", "t"]);
    std::fs::write(sub_staging.join("FILE"), b"sub-content").unwrap();
    run_git(&sub_staging, &["add", "FILE"]);
    run_git(&sub_staging, &["commit", "--quiet", "-m", "sub initial"]);
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
    run_git(&staging, &["config", "user.email", "t@t"]);
    run_git(&staging, &["config", "user.name", "t"]);
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
    run_git(&staging, &["commit", "--quiet", "-m", "with submodule"]);
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
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "feat-1", "--group", "ios"])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap();
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
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", "feat-2", "--group", "ios"])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap();
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
/// ref for `wt sync` to fetch by. Release un-creates it.
///
/// See CLAUDE.md §Lifecycle invariants step 11 + §Sync flow.
#[test]
fn acquire_branches_submodule_release_cleans_up() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());
    init_pool(&key, &bare);

    let slot_name = "feat-branch";
    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "acquire", "--name", slot_name, "--group", "ios"])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap();
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
        run_git(&sub_staging, &["config", "user.email", "t@t"]);
        run_git(&sub_staging, &["config", "user.name", "t"]);
        std::fs::write(sub_staging.join("FILE"), format!("sub{i}-content").as_bytes()).unwrap();
        run_git(&sub_staging, &["add", "FILE"]);
        run_git(&sub_staging, &["commit", "--quiet", "-m", "init"]);
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
    run_git(&staging, &["config", "user.email", "t@t"]);
    run_git(&staging, &["config", "user.name", "t"]);
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
    run_git(&staging, &["commit", "--quiet", "-m", "with submodules"]);
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
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
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
    cmd
}

/// Acquire a slot, then break it. `gitlink_only=true` mirrors the most common
/// ghost-dir state (the gitlink file is gone, source-repo admin still around);
/// `false` is the harder case where the source-repo admin was also pruned —
/// matters because git's upward-walk on a missing `.git` could otherwise let
/// `slot_repo_ok` falsely pass (e.g. dotfiles repo at $HOME).
fn acquire_then_break(key: &str, name: &str, gitlink_only: bool) -> PathBuf {
    let out = acquire_dev(key, name);
    assert!(out.status.success(), "acquire failed: {}", String::from_utf8_lossy(&out.stderr));
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
    assert!(out.status.success(), "acquire failed: {}", String::from_utf8_lossy(&out.stderr));
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
    assert!(out.status.success(),
        "rm --force should succeed on dirty slot.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
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
    run_git(&path, &["config", "user.email", "t@t"]);
    run_git(&path, &["config", "user.name", "t"]);
    std::fs::write(path.join("CHANGE"), b"new\n").unwrap();
    run_git(&path, &["add", "CHANGE"]);
    run_git(&path, &["commit", "--quiet", "-m", "ahead of main"]);

    // Without --force: refuse.
    let out = session_cmd(&key, &["rm", "ahead"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not in main"), "got: {stderr}");

    // With --force: succeed.
    let out = session_cmd(&key, &["rm", "ahead", "--force"]).output().unwrap();
    assert!(out.status.success(),
        "rm --force should discard unmerged commits.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
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
    assert!(out.status.success(),
        "rm --force <name> should succeed.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
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
/// auto-resolve the pool key from cwd (e.g. `wt sync`, which takes no `--pool`).
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
fn session_sync_preserves_untracked_in_main_on_collision() {
    // Regression: `reset --hard` silently clobbered untracked main scratch
    // at colliding paths; `merge --ff-only` refuses instead.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // `wt sync` requires main to be checked out as a worktree of source. The
    // bare fixture has none by default, so wire one alongside.
    let main_path = tmp.path().join("main-wt");
    run_git_root(&[
        "-C", &bare.display().to_string(),
        "worktree", "add", "--quiet",
        &main_path.display().to_string(), "main",
    ]);

    let out = acquire_dev(&key, "feat");
    assert!(out.status.success(), "acquire: {}", String::from_utf8_lossy(&out.stderr));
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    // Slot adds tracked COLLIDE.
    run_git(&slot_path, &["config", "user.email", "t@t"]);
    run_git(&slot_path, &["config", "user.name", "t"]);
    std::fs::write(slot_path.join("COLLIDE"), b"slot version\n").unwrap();
    run_git(&slot_path, &["add", "COLLIDE"]);
    run_git(&slot_path, &["commit", "--quiet", "-m", "add collide"]);

    // Operator scratches at the same path in main worktree (untracked).
    let scratch = b"operator scratch -- must not be lost\n";
    std::fs::write(main_path.join("COLLIDE"), scratch).unwrap();

    let out = session_cmd_cwd(&slot_path, &["sync"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Sync must refuse loudly (not silently advance main + clobber).
    assert!(!out.status.success(),
        "sync should refuse on untracked-file collision in main; \
         stdout={stdout}\nstderr={stderr}");

    // Untracked file is the load-bearing assertion: refusal w/o preservation
    // would still be a regression.
    let preserved = std::fs::read(main_path.join("COLLIDE"))
        .expect("untracked file deleted by sync");
    assert_eq!(preserved, scratch,
        "untracked file in main was overwritten; stdout={stdout}\nstderr={stderr}");
}

#[test]
fn session_sync_advances_main_on_clean_path() {
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

    run_git(&slot_path, &["config", "user.email", "t@t"]);
    run_git(&slot_path, &["config", "user.name", "t"]);
    std::fs::write(slot_path.join("NEW"), b"slot-added\n").unwrap();
    run_git(&slot_path, &["add", "NEW"]);
    run_git(&slot_path, &["commit", "--quiet", "-m", "add new"]);

    let out = session_cmd_cwd(&slot_path, &["sync"]).output().unwrap();
    assert!(out.status.success(),
        "sync should succeed; stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    // Tree-landed in main implies ref advanced (ff-only is atomic).
    let landed = std::fs::read(main_path.join("NEW")).expect("NEW missing in main");
    assert_eq!(landed, b"slot-added\n");
}

#[test]
fn session_sync_is_idempotent_on_rerun() {
    // Running sync twice in a row: second run hits the `main_before == slot_head`
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
    run_git(&slot_path, &["config", "user.email", "t@t"]);
    run_git(&slot_path, &["config", "user.name", "t"]);
    std::fs::write(slot_path.join("F"), b"f\n").unwrap();
    run_git(&slot_path, &["add", "F"]);
    run_git(&slot_path, &["commit", "--quiet", "-m", "f"]);

    assert!(session_cmd_cwd(&slot_path, &["sync"]).output().unwrap().status.success());
    let out = session_cmd_cwd(&slot_path, &["sync"]).output().unwrap();
    assert!(out.status.success(),
        "second sync should no-op cleanly; stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

#[test]
fn session_sync_refuses_when_main_worktree_on_other_branch() {
    // Operator detour: if main_path has a different branch checked out, sync
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
    run_git(&slot_path, &["config", "user.email", "t@t"]);
    run_git(&slot_path, &["config", "user.name", "t"]);
    std::fs::write(slot_path.join("X"), b"x\n").unwrap();
    run_git(&slot_path, &["add", "X"]);
    run_git(&slot_path, &["commit", "--quiet", "-m", "x"]);

    // `find_main_path` parses `worktree list --porcelain` for `branch refs/heads/main`;
    // after the operator's checkout, that line is gone, so sync errors with
    // "main is not checked out in any worktree". Either that or the symref
    // guard would catch it — load-bearing assertion is the operator branch
    // didn't move.
    let main_before = StdCommand::new("git")
        .args(["-C", &main_path.display().to_string(), "rev-parse", "operator-side"])
        .output().unwrap();
    let out = session_cmd_cwd(&slot_path, &["sync"]).output().unwrap();
    assert!(!out.status.success(),
        "sync must refuse; stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let main_after = StdCommand::new("git")
        .args(["-C", &main_path.display().to_string(), "rev-parse", "operator-side"])
        .output().unwrap();
    assert_eq!(main_before.stdout, main_after.stdout,
        "operator-side branch must not move");
}

