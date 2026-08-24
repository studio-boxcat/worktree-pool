//! Acquire/release lifecycle, capacity, exit codes, recycling, hooks, and races.
mod common;
use common::*;

use std::path::PathBuf;
use std::sync::{Arc, Barrier};

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
    // Canonical-only: slot stays at `{group}-N`, never the lease.
    assert!(path.ends_with("/ios-0"), "expected canonical ios-0 path, got: {path}");

    let ls = wtp()
        .args(["--pool", &key, "ls"])
        .output()
        .unwrap();
    let ls_text = String::from_utf8_lossy(&ls.stdout);
    assert!(ls_text.contains("feat-x"), "ls should mention the lease");
    assert!(ls_text.contains("held"));

    let inspect = wtp()
        .args(["--pool", &key, "inspect", "--lease", "feat-x"])
        .output()
        .unwrap();
    let inspect_text = String::from_utf8_lossy(&inspect.stdout);
    assert!(inspect_text.contains("sha:"), "inspect should show sha");
    assert!(inspect_text.contains("group: ios"));

    release(&key, "feat-x");

    // Idempotent re-release.
    wtp()
        .args(["--pool", &key, "release", "--lease", "feat-x"])
        .assert()
        .success();
}

/// `ExitKind::Contended` (exit code 3): when every candidate init mutex is
/// held by another process, acquire must exit 3 (not the generic 1 or the
/// capacity-4) so retry-aware callers can distinguish "transient — retry"
/// from "no capacity — release something". Locks the contract from
/// docs/cli.md §Exit codes.
#[test]
fn contended_init_mutex_exits_3() {
    use std::fs::{File, OpenOptions};

    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool_groupless_max1(&key, &bare);

    // Hold the only candidate's init-mutex flock from the test process. The
    // acquire subprocess opens the same path (different OFD) and `try_lock`
    // returns WouldBlock → FileLock::try_acquire returns None → no candidate
    // wins → bail_exit!(Contended).
    let mutex_path = pool_root(&key).join(".meta/init/slot-0.lock");
    std::fs::create_dir_all(mutex_path.parent().unwrap()).unwrap();
    let holder: File = OpenOptions::new()
        .read(true).write(true).create(true).truncate(false)
        .open(&mutex_path).unwrap();
    holder.try_lock().expect("test process must take the flock");

    let out = acquire_dev_groupless(&key, "blocked");
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(3),
        "init-mutex contention must exit 3; got {:?}", out.status.code());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("init mutexes held"),
        "expected contended-message; got: {stderr}");

    drop(holder);
}

/// Groupless pool: slots are named `slot-N` and `--group` is rejected.
#[test]
fn groupless_pool_full_lifecycle() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool_groupless(&key, &bare);

    // Acquire (no --group) → slot-0.
    let out = wtp()
        .args(["--pool", &key, "acquire", "--lease", "feat-x"])
        .output()
        .unwrap();
    assert_ok(&out, "groupless acquire");
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(path.ends_with("/slot-0"), "expected slot-0, got: {path}");

    // --group on a groupless pool is rejected.
    let out = wtp()
        .args(["--pool", &key, "acquire", "--lease", "feat-y", "--group", "ios"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no groups configured"),
        "expected group-refusal; got: {stderr}");

    // Release by lease.
    release(&key, "feat-x");

    // Recycle: re-acquire lands at the same slot-0.
    let out = wtp()
        .args(["--pool", &key, "acquire", "--lease", "feat-z"])
        .output()
        .unwrap();
    assert_ok(&out, "groupless re-acquire");
    let path2 = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(path, path2, "recycled slot must reuse the same canonical path");
}

#[test]
fn acquire_refuses_a_lease_already_held() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    assert_ok(&acquire_dev(&key, "build-1"), "first acquire");

    // A lease identifies exactly one slot. Without this check the second acquire would
    // succeed — plumbing bypasses git's same-branch-in-two-worktrees guard — and `release`
    // would then resolve it to an arbitrary one of the two.
    let dup = acquire_dev(&key, "build-1");
    assert!(!dup.status.success());
    assert_eq!(dup.status.code(), Some(6),
        "duplicate lease must exit 6; got {:?}", dup.status.code());
    let stderr = String::from_utf8_lossy(&dup.stderr);
    assert!(stderr.contains("already held"), "got: {stderr}");

    // A distinct lease in a sibling group is unaffected.
    let other = wtp()
        .args(["--pool", &key, "acquire", "--lease", "build-2", "--group", "android"])
        .output()
        .unwrap();
    assert_ok(&other, "distinct lease");

    // Released leases are reusable.
    release(&key, "build-1");
    assert_ok(&acquire_dev(&key, "build-1"), "re-acquire after release");
}

#[test]
fn refuses_uninitialized_pool() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let out = wtp()
        .args(["--pool", &key, "ls"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not initialized"));
}

#[test]
fn doctor_runs_without_pool() {
    let out = wtp()
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

    let out = wtp()
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
    assert_ok(&acquire_dev(&key, "feat-x"), "acquire feat-x");

    let out = wtp()
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
    // isolation). The load-bearing assertion is "different slots picked".
    let mut outs = Vec::new();
    for i in 0..3 {
        let out = acquire_dev(&key, &format!("dev-{i}"));
        assert_ok(&out, "acquire failed");
        outs.push(output_to_slot_path(&out));
    }
    let paths: std::collections::HashSet<_> = outs.iter().cloned().collect();
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
    wtp()
        .args(["--pool", &key, "init"])
        .arg("--source")
        .arg(&bare)
        .args(["--max-slots", "2", "--groups", "ios"])
        .assert()
        .success();

    assert_ok(&acquire_dev(&key, "a"), "acquire a");
    assert_ok(&acquire_dev(&key, "b"), "acquire b");
    let out = acquire_dev(&key, "c");
    assert!(!out.status.success());
    // Exit 4 = ExitKind::Capacity — distinguishes "all slots held" from
    // transient contention or a held lease, for retry-aware callers.
    assert_eq!(out.status.code(), Some(4),
        "capacity exhaustion must exit 4; got {:?}", out.status.code());
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
            wtp()
                .args(["--pool", &key, "release", "--lease", name])
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
    let ls = wtp()
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

/// Distinct leases at one SHA are independent work and must both succeed — this is what a
/// build's player and bundles halves do, and what the two platforms do off every release
/// commit. Guards against reintroducing a commit-keyed exclusion.
#[test]
fn distinct_leases_at_one_sha_both_acquire() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out1 = wtp()
        .args([
            "--pool", &key, "acquire", "--lease", "b-0",
            "--commit", "main", "--group", "ios",
        ])
        .output()
        .unwrap();
    assert_ok(&out1, "first acquire");
    let out2 = wtp()
        .args([
            "--pool", &key, "acquire", "--lease", "b-1",
            "--commit", "main", "--group", "ios",
        ])
        .output()
        .unwrap();
    assert_ok(&out2, "distinct lease at the same SHA");

    // Distinct slots, so neither can disturb the other's checkout.
    assert_ne!(
        String::from_utf8_lossy(&out1.stdout).trim(),
        String::from_utf8_lossy(&out2.stdout).trim(),
        "distinct leases must land on distinct slots"
    );
}

// ---------- wt_post_acquire hook (core-fired) ----------

/// A defined `wt_post_acquire` fires in the slot after acquire: its marker lands
/// in the slot checkout, acquire still succeeds, and stdout is the slot path.
#[test]
fn post_acquire_hook_fires_in_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    // Relative `HOOK_MARKER` proves cwd == slot; body captures the WT_* contract.
    let bare = make_fixture_with_hook(
        tmp.path(),
        "wt_post_acquire() { printf '%s|%s' \"$WT_KEY\" \"$WT_LEASE\" > HOOK_MARKER; }\n",
    );
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-hook");
    assert_ok(&out, "acquire with hook");
    let slot = output_to_slot_path(&out);

    let marker = slot.join("HOOK_MARKER");
    assert!(marker.exists(), "hook marker missing in slot: {}", marker.display());
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("{key}|feat-hook"),
        "hook saw wrong WT_KEY/WT_LEASE contract"
    );
}

/// A chatty hook must not pollute stdout: build-pool callers parse acquire's
/// stdout as the slot path, so the hook's stdout is routed to stderr. stdout
/// must be exactly the slot path even when the hook echoes.
#[test]
fn post_acquire_hook_stdout_does_not_pollute_path() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_hook(
        tmp.path(),
        "wt_post_acquire() { echo noise-on-stdout; }\n",
    );
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-chatty");
    assert_ok(&out, "acquire with chatty hook");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // stdout is a single line — the slot path — with no hook noise.
    assert!(!stdout.contains("noise-on-stdout"),
        "hook stdout leaked into acquire stdout: {stdout}");
    assert_eq!(stdout.lines().count(), 1,
        "acquire stdout must be exactly the slot path; got: {stdout}");
    assert!(stdout.trim().ends_with("/ios-0"),
        "stdout should be the canonical slot path; got: {stdout}");
    // The hook's chatter landed on stderr instead.
    assert!(stderr.contains("noise-on-stdout"),
        "hook stdout should be redirected to stderr; stderr: {stderr}");
}

/// A failing `wt_post_acquire` is fail-loud: acquire exits non-zero (the gate
/// that stops a rejecting hook from yielding a usable slot).
#[test]
fn post_acquire_hook_failure_fails_acquire() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_hook(tmp.path(), "wt_post_acquire() { exit 1; }\n");
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-reject");
    assert!(
        !out.status.success(),
        "acquire must fail when wt_post_acquire exits non-zero; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("wt_post_acquire") || stderr.contains("post-acquire hook"),
        "expected post-acquire hook failure message; got: {stderr}"
    );
}

#[test]
fn release_succeeds_despite_stale_index_lock() {
    // Reproduces the meow-tower 2026-05-18 warn: a crashed git left a
    // `<gitdir>/index.lock`, then release's old `git checkout --detach` step hit
    // `EEXIST` on the lock and the slot's branch persisted as a dangling ref.
    // The fix (release.rs → `git::detach_head` via plumbing rev-parse +
    // update-ref --no-deref) never touches the index, so the lock is irrelevant
    // to release — no lock sweep needed on this path.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "leaky");
    assert_ok(&out, "acquire failed");
    let slot = output_to_slot_path(&out);

    // Fabricate the crashed-git leftover: non-empty (escapes 0-byte guard) and
    // freshly mtime'd (escapes the 60s age guard). Either condition alone keeps
    // it from being swept; both make the test deterministic.
    let gitdir = slot_gitdir_path(&slot);
    let index_lock = gitdir.join("index.lock");
    std::fs::write(&index_lock, b"partial write before SIGKILL\n").unwrap();

    // Invoke the binary directly: the `wt` wrapper would itself run `git status
    // --porcelain` for the dirty-tree precheck and could race the same lock,
    // muddying what this test is meant to pin (release.rs's detach step).
    let out = wtp()
        .args(["--pool", &key, "release", "--lease", "leaky"])
        .output().unwrap();
    assert_ok(&out, "release should succeed despite stale index.lock");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("may persist as a dangling ref"),
        "release should not hit detach-failure warn (the bug being fixed): {stderr}");
    // Slot is now idle (detached HEAD).
    assert_head_detached(&slot);

    // Branch should be gone — confirms the detach actually freed the ref for
    // the subsequent `branch -D`. (If detach silently no-op'd, `branch -D`
    // would refuse because we'd still be ON `leaky`.)
    let branches = run_git_capture(&bare, &["branch", "--list", "leaky"]);
    assert!(branches.trim().is_empty(), "'leaky' branch should be deleted; got: {branches:?}");
}

#[test]
fn release_converges_and_slot_recycles() {
    // Release detaches HEAD + deletes the branch, flipping held → idle.
    // Verify the full release-then-reacquire cycle converges.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-half");
    assert_ok(&out, "");
    let slot = output_to_slot_path(&out);

    // Slot is held (on branch feat-half).
    let head = run_git_capture(&slot, &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(head.trim(), "feat-half");

    // Release converges.
    release(&key, "feat-half");

    // Slot is now idle (detached HEAD). Branch is deleted.
    assert_head_detached(&slot);
    let branches = run_git_capture(&bare, &["branch", "--list", "feat-half"]);
    assert!(branches.trim().is_empty(), "'feat-half' branch should be deleted");

    let out = acquire_dev(&key, "feat-after");
    assert_ok(&out, "");
}

#[test]
fn acquire_does_not_disturb_live_held_slot() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "live-1");
    assert_ok(&out, "");
    let live_slot = output_to_slot_path(&out);

    let out = acquire_dev(&key, "live-2");
    assert_ok(&out, "");

    assert!(live_slot.exists(), "live held slot dir must remain");
    let head = run_git_capture(&live_slot, &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(head.trim(), "live-1", "live slot's HEAD must still be on its branch");
}

/// A crashed git (e.g. lazygit) can leave a `<gitdir>/index.lock` in a slot.
/// Once the slot is released (idle), the next acquire recycles it with `git
/// reset --hard`, which writes the index and would fail with EEXIST on the
/// leftover lock. acquire removes the lock first — the slot is idle under the
/// pool + init mutex, so the remove is race-free. Verify recycle succeeds and
/// the lock is gone.
#[test]
fn recycled_acquire_clears_leftover_index_lock() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare);

    let out = acquire_dev(&key, "feat-first");
    assert_ok(&out, "");
    let slot = output_to_slot_path(&out);
    let gitdir = slot_gitdir_path(&slot);

    // Release flips the slot idle (detached HEAD) without touching the index.
    release(&key, "feat-first");

    // Fabricate a crashed-git leftover in the now-idle slot's gitdir. Non-empty
    // + fresh, so the old 0-byte+age heuristic would NOT have swept it — proves
    // the recycle-time remove is unconditional.
    let lock = gitdir.join("index.lock");
    std::fs::write(&lock, b"partial write before SIGKILL\n").unwrap();

    // Next acquire recycles the same canonical slot (lowest-N-first).
    let out = acquire_dev(&key, "feat-recycle");
    assert_ok(&out, "recycled acquire must succeed despite a leftover index.lock");
    let recycled = output_to_slot_path(&out);
    assert_eq!(recycled, slot, "should recycle the same canonical slot");
    assert!(!lock.exists(),
        "acquire must remove the leftover index.lock before reset --hard");
}
