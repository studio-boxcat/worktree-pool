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
        .args(["--pool", key, "acquire", name, "--group", "ios"])
        .output()
        .unwrap()
}

fn acquire_dev_sub(key: &str, name: &str) -> std::process::Output {
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", key, "acquire", name, "--group", "ios"])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap()
}

fn release(key: &str, name: &str) {
    Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", key, "release", name])
        .assert()
        .success();
}

/// Extract the canonical slot id (basename of acquire's stdout path).
fn slot_id_from_output(out: &std::process::Output) -> String {
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    PathBuf::from(&path).file_name().unwrap().to_string_lossy().into_owned()
}

/// Init-mutex flock guard. The OS releases the flock on drop (file close).
///
/// **Why tests need this:** acquire/release CLI processes exit promptly, so
/// the OS frees their init-mutex flock immediately on return. The next
/// pool-mutex op runs `reclaim_stale`, sees a held slot with no live holder,
/// and replays release — silently un-holding the slot the test just acquired.
/// Tests that need a slot to remain held across multiple worktree-pool CLI
/// invocations grab this guard *after* acquire returns to keep the slot live.
struct InitMutexHold {
    _file: std::fs::File,
}

impl InitMutexHold {
    fn acquire(key: &str, slot_id: &str) -> Self {
        let path = pool_root(key).join(".meta/init").join(format!("{slot_id}.lock"));
        let file = std::fs::OpenOptions::new()
            .read(true).write(true).create(true).truncate(false)
            .open(&path)
            .unwrap_or_else(|e| panic!("open init mutex {}: {e}", path.display()));
        file.try_lock()
            .unwrap_or_else(|e| panic!("flock init mutex {}: {e}", path.display()));
        Self { _file: file }
    }
}

/// Acquire a slot AND hold its init-mutex flock so reclaim_stale won't sweep
/// it between CLI invocations. Returns (slot_path, flock_guard).
fn acquire_held(key: &str, name: &str) -> (PathBuf, InitMutexHold) {
    let out = acquire_dev(key, name);
    assert_ok(&out, "acquire failed");
    let slot_id = slot_id_from_output(&out);
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let hold = InitMutexHold::acquire(key, &slot_id);
    (path, hold)
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
    // Canonical-only: slot stays at `{group}-N`, never `<name>`.
    assert!(path.ends_with("/ios-0"), "expected canonical ios-0 path, got: {path}");

    let ls = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "ls"])
        .output()
        .unwrap();
    let ls_text = String::from_utf8_lossy(&ls.stdout);
    assert!(ls_text.contains("feat-x"), "ls should mention branch name 'feat-x'");
    assert!(ls_text.contains("held"));

    let inspect = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "inspect", "feat-x"])
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
        .args(["--pool", &key, "release", "feat-x"])
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

    let out1 = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "build-1",
            "--commit", "main", "--group", "ios", "--unique-sha",
        ])
        .output()
        .unwrap();
    assert_ok(&out1, "first acquire");
    // Hold init-mutex flock so subsequent reclaim_stale sees the slot as live.
    let _hold1 = InitMutexHold::acquire(&key, &slot_id_from_output(&out1));

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "build-2",
            "--commit", "main", "--group", "ios", "--unique-sha",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("full_sha"));
    assert!(stderr.contains("build-1"));

    // Dev acquire (no --unique-sha) is allowed.
    let out_dev = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "dev-foo",
            "--commit", "main", "--group", "ios",
        ])
        .output()
        .unwrap();
    assert_ok(&out_dev, "dev acquire");
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
fn unstick_reports_init_mutex_state() {
    // Under fs-flock: leftover mutex *files* carry no semantic load — flock is
    // the source of truth. `unstick` is now a read-only diagnostic.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    // Plant a leftover mutex file (no flock held).
    let init_dir = pool_root(&key).join(".meta/init");
    std::fs::create_dir_all(&init_dir).unwrap();
    let leftover = init_dir.join("ios-2.lock");
    std::fs::write(&leftover, b"").unwrap();

    let out = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args(["--pool", &key, "unstick"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("init mutex ios-2: free"),
        "expected diagnostic line for ios-2; got: {stdout}");
    assert!(stdout.contains("unstick:") && stdout.contains("total"),
        "expected summary line; got: {stdout}");
    // The file is left in place — flock is the source of truth, not the file.
    assert!(leftover.exists(),
        "unstick is read-only; mutex file should remain (got removed)");
}

#[test]
fn ls_renders_with_git_status_for_held_only() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);
    let (_, _hold) = acquire_held(&key, "feat-x");

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
    // Slot is at ios-0; branch name 'feat-x' should appear in the row.
    let row = stdout.lines().find(|l| l.contains("feat-x")).unwrap();
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

    // pool_mutex serializes acquires, so true parallelism manifests only as
    // contention on that mutex (each acquire then runs to completion in
    // isolation). The load-bearing assertion is "different slots picked",
    // which we keep — but each acquire's init-mutex flock is grabbed
    // synchronously so the next acquire's reclaim_stale sees it as live.
    let mut outs = Vec::new();
    let mut holds = Vec::new();
    for i in 0..3 {
        let (path, hold) = acquire_held(&key, &format!("dev-{i}"));
        outs.push(path);
        holds.push(hold);
    }
    let paths: std::collections::HashSet<_> = outs.iter().cloned().collect();
    assert_eq!(paths.len(), 3, "duplicate slot paths: {paths:?}");
    drop(holds);
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

    let (_, _h1) = acquire_held(&key, "a");
    let (_, _h2) = acquire_held(&key, "b");
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
                .args(["--pool", &key, "release", name])
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

/// `--unique-sha` race coverage: sequential, with a flock-held first holder
/// to keep the slot "live" across CLI invocations. (True parallelism isn't
/// observable here — pool_mutex serializes acquires; the parallel-flavored
/// flock-handoff race needs a long-running holder rather than two short CLI
/// invocations.)
#[test]
fn parallel_unique_sha_at_most_one_succeeds() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out1 = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "b-0",
            "--commit", "main", "--group", "ios", "--unique-sha",
        ])
        .output()
        .unwrap();
    assert_ok(&out1, "first --unique-sha acquire");
    let _hold = InitMutexHold::acquire(&key, &slot_id_from_output(&out1));

    let out2 = Command::cargo_bin("worktree-pool")
        .unwrap()
        .args([
            "--pool", &key, "acquire", "b-1",
            "--commit", "main", "--group", "ios", "--unique-sha",
        ])
        .output()
        .unwrap();
    assert!(!out2.status.success(),
        "second --unique-sha acquire on same SHA should fail; stderr={}",
        String::from_utf8_lossy(&out2.stderr));
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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let _hold = InitMutexHold::acquire(&key, &slot_id_from_output(&out));
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
        .args(["--pool", &key, "acquire", "many", "--group", "ios"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "acquire failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());

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
///
/// Returns (slot_path, canonical_slot_id). Under canonical-only naming, wt's
/// dir-based lookups (cmd_go/cmd_release/cmd_cleanup) key off the slot id.
fn acquire_then_break(key: &str, name: &str, gitlink_only: bool) -> (PathBuf, String) {
    let out = acquire_dev(key, name);
    assert_ok(&out, "acquire failed");
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let slot_id = slot_id_from_output(&out);
    assert!(path.exists());
    let gitlink = std::fs::read_to_string(path.join(".git")).expect("read gitlink");
    let admin = PathBuf::from(gitlink.strip_prefix("gitdir: ").unwrap().trim());
    std::fs::remove_file(path.join(".git")).expect("remove slot .git gitlink");
    if !gitlink_only {
        std::fs::remove_dir_all(&admin).expect("remove source-repo admin");
    }
    (path, slot_id)
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

    let (path, slot_id) = acquire_then_break(&key, "ghost", true);

    let out = session_cmd(&key, &["go", &slot_id]).output().unwrap();
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

    let (path, slot_id) = acquire_then_break(&key, "ghost", true);

    let out = session_cmd(&key, &["cleanup", &slot_id]).output().unwrap();
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

    let (path, slot_id) = acquire_then_break(&key, "ghost", false);

    let out = session_cmd(&key, &["cleanup", &slot_id]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stdout.contains("recycled cleanly"),
        "stdout={stdout}\nstderr={stderr}");
    assert!(stderr.contains("🔴 BROKEN:"),
        "stderr={stderr}");
    assert!(path.exists());
}

#[test]
fn session_release_refuses_broken_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let (path, slot_id) = acquire_then_break(&key, "ghost", true);

    let out = session_cmd(&key, &["release", &slot_id]).output().unwrap();
    assert!(!out.status.success(),
        "release should refuse broken slot rather than dive into worktree-pool release.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is broken (no valid .git"),
        "expected guarded refusal; got: {stderr}");
    assert!(path.exists());
}

/// Acquire a slot and dirty its working tree (tracked + untracked changes).
/// Returns (slot_path, canonical_slot_id).
fn acquire_then_dirty(key: &str, name: &str) -> (PathBuf, String) {
    let out = acquire_dev(key, name);
    assert_ok(&out, "acquire failed");
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let slot_id = slot_id_from_output(&out);
    std::fs::write(path.join("README"), b"dirty tracked\n").unwrap();
    std::fs::write(path.join("untracked.txt"), b"new\n").unwrap();
    (path, slot_id)
}

#[test]
fn session_release_refuses_dirty_without_force() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let (path, slot_id) = acquire_then_dirty(&key, "messy");

    let out = session_cmd(&key, &["release", &slot_id]).output().unwrap();
    assert!(!out.status.success(),
        "release should refuse dirty slot without --force.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("dirty"), "stderr should mention dirty: {stderr}");
    assert!(path.exists());
}

#[test]
fn session_release_force_discards_dirty() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let (path, slot_id) = acquire_then_dirty(&key, "messy");

    let out = session_cmd(&key, &["release", &slot_id, "--force"]).output().unwrap();
    assert_ok(&out, "release --force should succeed on dirty slot");
    // Canonical slot dir persists (it's the warm-cache home); the lock file
    // is what flips from held → idle. Assert the slot is now idle.
    assert!(path.exists(), "canonical slot dir should remain (warm cache)");
}

#[test]
fn session_release_force_accepted_before_positionals() {
    // Pin the parser contract: `release --force <name>` works as well as
    // `release <name> --force`. Operators reach for either order.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let (path, slot_id) = acquire_then_dirty(&key, "messy");
    let out = session_cmd(&key, &["release", "--force", &slot_id]).output().unwrap();
    assert_ok(&out, "release --force <name> should succeed");
    assert!(path.exists(), "canonical slot dir should remain (warm cache)");
}

#[test]
fn session_release_force_still_refuses_broken_slot() {
    // --force means "discard work I know is junk" — it does NOT mean
    // "rm -rf a path that can't be released through git". Broken slots
    // need explicit `rm -rf` (no git state to release).
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let (path, slot_id) = acquire_then_break(&key, "ghost", true);

    let out = session_cmd(&key, &["release", &slot_id, "--force"]).output().unwrap();
    assert!(!out.status.success(),
        "release --force must still refuse broken slot.\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("is broken (no valid .git"), "got: {stderr}");
    assert!(path.exists());
}

#[test]
fn release_succeeds_despite_stale_index_lock() {
    // Reproduces the meow-tower 2026-05-18 warn: a crashed git left a non-empty,
    // recent `<gitdir>/index.lock` that escaped `reclaim_stale`'s 0-byte+age
    // guard, then release's old `git checkout --detach` step hit `EEXIST` on
    // the lock and the slot's branch persisted as a dangling ref. The fix
    // (release.rs → `git::detach_head` via plumbing rev-parse + update-ref
    // --no-deref) never touches the index, so the lock is irrelevant.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "leaky");
    assert_ok(&out, "acquire failed");
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    // Fabricate the crashed-git leftover: non-empty (escapes 0-byte guard) and
    // freshly mtime'd (escapes the 60s age guard). Either condition alone keeps
    // it from being swept; both make the test deterministic.
    let gitdir = slot_gitdir_path(&slot);
    let index_lock = gitdir.join("index.lock");
    std::fs::write(&index_lock, b"partial write before SIGKILL\n").unwrap();

    // Invoke the binary directly: the `wt` wrapper would itself run `git status
    // --porcelain` for the dirty-tree precheck and could race the same lock,
    // muddying what this test is meant to pin (release.rs's detach step).
    let out = Command::cargo_bin("worktree-pool").unwrap()
        .args(["--pool", &key, "release", "leaky"])
        .output().unwrap();
    assert_ok(&out, "release should succeed despite stale index.lock");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("may persist as a dangling ref"),
        "release should not hit detach-failure warn (the bug being fixed): {stderr}");
    // Canonical slot dir persists; held → idle is signaled by lock removal.
    let gitdir = slot_gitdir_path(&slot);
    assert!(!gitdir.join("worktree-pool/lock").exists(),
        "slot lock should be gone after release");

    // Branch should be gone — confirms the detach actually freed the ref for
    // the subsequent `branch -D`. (If detach silently no-op'd, `branch -D`
    // would refuse because we'd still be ON `leaky`.)
    let branches = run_git_capture(&bare, &["branch", "--list", "leaky"]);
    assert!(branches.trim().is_empty(), "'leaky' branch should be deleted; got: {branches:?}");
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
    let _hold_a = InitMutexHold::acquire(&key, &slot_id_from_output(&out_a));
    let out_b = acquire_dev(&key, "slot-b");
    assert_ok(&out_b, "acquire slot-b");
    let slot_b = PathBuf::from(String::from_utf8_lossy(&out_b.stdout).trim().to_string());
    assert_ne!(slot_a, slot_b, "slot-a and slot-b must be different canonical slots");

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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot_a = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let _hold_a = InitMutexHold::acquire(&key, &slot_id_from_output(&out));
    let out = acquire_dev_sub(&key, "slot-b");
    assert_ok(&out, "acquire slot-b");
    let slot_b = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

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
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

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
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot_path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
fn release_replay_completes_after_slow_ops_crash() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-half");
    assert_ok(&out, "");
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let slot_id = slot_id_from_output(&out);
    run_git(&slot, &["checkout", "--quiet", "--detach"]);
    run_git(&slot, &["branch", "-D", "feat-half"]);
    let gitdir = slot_gitdir_path(&slot);
    assert!(gitdir.join("worktree-pool/lock").exists(),
        "lock still present (new-ordering crash invariant)");

    // Branch is gone, so release by branch name finds nothing. Operator
    // recovers by passing the canonical slot id instead — release's
    // find_by_name → canonical-path fallback path.
    release(&key, &slot_id);

    // Canonical slot dir persists; held → idle is signaled by lock removal.
    assert!(!gitdir.join("worktree-pool/lock").exists(),
        "lock should be gone after release by canonical slot id");
    let out = acquire_dev(&key, "feat-after");
    assert_ok(&out, "");
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
    let live_slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
    let _hold = InitMutexHold::acquire(&key, &slot_id_from_output(&out));
    let live_gitdir = slot_gitdir_path(&live_slot);
    let live_lock_before = std::fs::read(live_gitdir.join("worktree-pool/lock")).unwrap();

    let out = acquire_dev(&key, "live-2");
    assert_ok(&out, "");

    assert!(live_slot.exists(), "live held slot dir must remain");
    let live_lock_after = std::fs::read(live_gitdir.join("worktree-pool/lock")).unwrap();
    assert_eq!(live_lock_before, live_lock_after, "live lock must not be touched");
    let head = run_git_capture(&live_slot, &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(head.trim(), "live-1", "live slot's HEAD must still be on its branch");
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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());

    // Submodule is initialized; acquire created a `feat-sub` branch in it.
    let sub = slot.join("sub");
    assert!(sub.join(".git").exists(), "submodule should be initialized");
    let sub_branches_before = run_git_capture(&sub, &["branch", "--list", "feat-sub"]);
    assert!(sub_branches_before.contains("feat-sub"),
        "submodule should have feat-sub branch: {sub_branches_before:?}");

    // Compose pre-release crash: lock still present, branch still on HEAD —
    // models crash AFTER acquire wrote the lock but BEFORE any release ran.
    // reclaim_stale should read the branch name from HEAD and complete the
    // recursive submodule branch_delete + lock removal.
    let gitdir = slot_gitdir_path(&slot);
    assert!(gitdir.join("worktree-pool/lock").exists());

    // Replay completes the submodule branch cleanup + lock removal.
    release(&key, "feat-sub");
    assert!(!gitdir.join("worktree-pool/lock").exists(),
        "lock should be gone after release replay");

    // Submodule's per-slot branch should be gone too.
    let sub_branches_after = run_git_capture(&sub, &["branch", "--list", "feat-sub"]);
    assert!(sub_branches_after.trim().is_empty(),
        "submodule's feat-sub branch should be cleaned: {sub_branches_after:?}");
}

// ---------- pool-ownership marker tests (provenance gate) ----------

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
        .args(["--pool", key, "release", "no-such-slot"])
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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
    let slot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string());
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
