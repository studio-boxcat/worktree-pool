//! `wt` bash wrapper: go/cleanup/release classifier + land flow (parent + submodules).
mod common;
use common::*;

use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::{Arc, Barrier};

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
    // Canonical slot dir persists (it's the warm-cache home). Assert the
    // slot is now idle (HEAD detached).
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
    let slot_path = output_to_slot_path(&out);

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
    let slot_path = output_to_slot_path(&out);

    std::fs::write(slot_path.join("NEW"), b"slot-added\n").unwrap();
    git_commit(&slot_path, "add new");

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land should succeed");

    // Tree-landed in main implies ref advanced (ff-only is atomic).
    let landed = std::fs::read(main_path.join("NEW")).expect("NEW missing in main");
    assert_eq!(landed, b"slot-added\n");
}

#[test]
fn session_land_clears_leftover_index_lock_before_commit() {
    // land auto-commits dirty tracked work (`git add -u` + commit), which writes
    // the index and would fail EEXIST on a leftover `index.lock` (e.g. from a
    // crashed lazygit). land removes the lock first — verify the land succeeds
    // and the lock is gone. Non-empty + fresh lock proves the remove is
    // unconditional (no staleness heuristic).
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
    let slot_path = output_to_slot_path(&out);

    // Dirty tracked change so land must auto-commit (which needs the index lock free).
    std::fs::write(slot_path.join("README"), b"changed\n").unwrap();

    // Plant a crashed-git leftover the auto-commit would otherwise trip over.
    let gitdir = slot_gitdir_path(&slot_path);
    let lock = gitdir.join("index.lock");
    std::fs::write(&lock, b"partial write before SIGKILL\n").unwrap();

    let out = session_cmd_cwd(&slot_path, &["land", "wip"]).output().unwrap();
    assert_ok(&out, "land must clear the leftover index.lock and commit");
    assert!(!lock.exists(),
        "land must remove the leftover index.lock before committing");

    // Dirty work landed in main (ff-only is atomic → ref advanced).
    let landed = std::fs::read(main_path.join("README")).expect("README missing in main");
    assert_eq!(landed, b"changed\n");
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
    let slot_path = output_to_slot_path(&out);
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
    let slot_a = output_to_slot_path(&out_a);
    let out_b = acquire_dev(&key, "slot-b");
    assert_ok(&out_b, "acquire slot-b");
    let slot_b = output_to_slot_path(&out_b);
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
    let slot = output_to_slot_path(&out);

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
    init_pool_mirror(&key, &source, &source);
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
    let slot = output_to_slot_path(&out);
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
              .lines().next().is_some_and(|p| std::path::Path::new(p).exists()),
        "fixture failed to leave MERGE_HEAD");

    let out = acquire_dev(&key, "feat");
    assert_ok(&out, "acquire");
    let slot = output_to_slot_path(&out);

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
    let slot_a = output_to_slot_path(&out);
    let out = acquire_dev(&key, "slot-b");
    assert_ok(&out, "acquire b");
    let slot_b = output_to_slot_path(&out);

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
    // Slot-direction refresh: slot A bumps the submodule + lands, slot B
    // (branched before A landed) does unrelated parent-side work + lands. B's
    // parent merge brings in main's new gitlink; B's submodule clone must be
    // updated to match (no phantom rewinds in `git status`), and the move must
    // be journaled in reflog. `submodule update` leaves it detached at the pin.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    run_git(&source, &["config", "submodule.sub.ignore", "untracked"]);
    init_pool_mirror(&key, &source, &source);

    let out = acquire_dev_sub(&key, "slot-a");
    assert_ok(&out, "acquire slot-a");
    let slot_a = output_to_slot_path(&out);
    let out = acquire_dev_sub(&key, "slot-b");
    assert_ok(&out, "acquire slot-b");
    let slot_b = output_to_slot_path(&out);

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
    init_pool_mirror(&key, &source, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = output_to_slot_path(&out);

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
    init_pool_mirror(&key, &source, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = output_to_slot_path(&out);
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
    init_pool_mirror(&key, &source, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert!(out.status.success());
    let slot_path = output_to_slot_path(&out);
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
fn session_land_advances_detached_submodule_in_main() {
    // Failure mode 2 (detached variant): main's submodule clone is detached at
    // the old pin; the slot advances the pin. Land fetches the new pin from the
    // slot's clone (LOCAL — HEAD, not the superproject branch name, which the
    // submodule clone lacks → the old "couldn't find remote ref" bug) and
    // fast-forwards main's clone. Detached HEAD is fine; the clone reaches the pin.
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    init_pool_mirror(&key, &source, &source);

    // Simulate `git submodule update` aftermath: detach source/sub at gitlink SHA.
    let main_sub = source.join("sub");
    let head_sha = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    let sha = String::from_utf8_lossy(&head_sha.stdout).trim().to_string();
    run_git(&main_sub, &["checkout", "--quiet", "--detach", &sha]);

    let out = acquire_dev_sub(&key, "feat");
    assert!(out.status.success());
    let slot_path = output_to_slot_path(&out);
    let slot_sub = slot_path.join("sub");

    // Bump on a detached HEAD so the slot-name branch doesn't carry the new pin
    // (reproduces the branch-name fetch bug — see session_land_advances_submodule_pin_bump).
    run_git(&slot_sub, &["checkout", "--quiet", "--detach"]);
    std::fs::write(slot_sub.join("NEW"), b"x\n").unwrap();
    git_commit(&slot_sub, "x");
    git_commit(&slot_path, "bump sub");

    let slot_sub_head = StdCommand::new("git")
        .args(["-C", &slot_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land");

    // main's submodule clone advanced to the pinned commit (detached is fine).
    let main_sub_head = StdCommand::new("git")
        .args(["-C", &main_sub.display().to_string(), "rev-parse", "HEAD"])
        .output().unwrap();
    assert_eq!(main_sub_head.stdout, slot_sub_head.stdout,
        "main's submodule clone did not advance to the pinned SHA");
    assert!(main_sub.join("NEW").exists(),
        "the bumped submodule's new file did not land in main's clone");
}

#[test]
fn session_land_advances_submodule_pin_bump() {
    // Failure mode 2 (canonical): an existing submodule's pin is advanced in the
    // slot to a commit main's clone has never seen. Land must fetch that commit
    // LOCALLY from the slot's clone before fast-forwarding — the pre-fix code
    // fetched the superproject branch name (absent in the submodule clone), so
    // main's clone never got the SHA and the ancestry check reported "diverged".
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging");
    init_pool_mirror(&key, &source, &source);

    let main_sub = source.join("sub");
    let old_pin = String::from_utf8_lossy(
        &StdCommand::new("git").args(["-C", &main_sub.display().to_string(), "rev-parse", "HEAD"])
            .output().unwrap().stdout).trim().to_string();

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = output_to_slot_path(&out);
    let slot_sub = slot_path.join("sub");

    // Advance the pin on a DETACHED HEAD so acquire's `<slot-name>` branch in
    // the submodule clone does NOT carry the new commit. That's what reproduces
    // the bug: the old code fetched that branch name (stale, missing the new
    // SHA) → main's clone never got it → spurious "diverged". (Verified: this
    // test fails on the pre-fix wt.)
    run_git(&slot_sub, &["checkout", "--quiet", "--detach"]);
    std::fs::write(slot_sub.join("BUMP"), b"bump\n").unwrap();
    git_commit(&slot_sub, "advance pin");
    git_commit(&slot_path, "bump sub pin");
    let new_pin = String::from_utf8_lossy(
        &StdCommand::new("git").args(["-C", &slot_sub.display().to_string(), "rev-parse", "HEAD"])
            .output().unwrap().stdout).trim().to_string();
    assert_ne!(old_pin, new_pin, "fixture: pin did not advance");

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land");

    // main's clone advanced to the new pin (no "diverged", no "couldn't find remote ref").
    let main_sub_now = String::from_utf8_lossy(
        &StdCommand::new("git").args(["-C", &main_sub.display().to_string(), "rev-parse", "HEAD"])
            .output().unwrap().stdout).trim().to_string();
    assert_eq!(main_sub_now, new_pin, "main's submodule clone did not advance to the new pin");
    assert!(main_sub.join("BUMP").exists(), "the bumped commit's file is missing from main's clone");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("couldn't find remote ref"),
        "land hit the branch-name fetch bug: {stderr}");
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
    init_pool_mirror(&key, &source, &source);

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire");
    let slot_path = output_to_slot_path(&out);
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

/// Regression: a feature branch that *introduces a brand-new submodule* must
/// land cleanly — `main` advances AND the new submodule is populated in the
/// main worktree. Pins the fix for the dead `land_die "main worktree's
/// submodule clone missing at ..."`, which left `main` frozen at the pre-merge
/// tip while the slot branch had already advanced (the pspec incident). The
/// populate sources from the slot's clone (local, no network) and restores the
/// declared origin so it survives slot recycle.
#[test]
fn session_land_populates_newly_introduced_submodule_in_main() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();

    // Source starts WITHOUT any submodule; `main` is checked out at `staging`.
    make_fixture(tmp.path());
    let source = tmp.path().join("staging");
    init_pool(&key, &source);

    // A separate bare repo to introduce as a submodule from inside the slot.
    let vendor_bare = tmp.path().join("vendor.git");
    run_git_root(&["init", "--quiet", "--bare", &vendor_bare.display().to_string()]);
    let vendor_staging = tmp.path().join("vendor-staging");
    run_git_root(&[
        "clone", "--quiet",
        &vendor_bare.display().to_string(),
        &vendor_staging.display().to_string(),
    ]);
    std::fs::write(vendor_staging.join("LIB"), b"vendor-content\n").unwrap();
    git_commit(&vendor_staging, "vendor init");
    run_git(&vendor_staging, &["push", "--quiet", "-u", "origin", "main"]);

    // Acquire a slot and introduce the submodule there.
    let out = acquire_dev_sub(&key, "add-vendor");
    assert_ok(&out, "acquire");
    let slot_path = output_to_slot_path(&out);
    run_git(
        &slot_path,
        &[
            "-c", "protocol.file.allow=always",
            "submodule", "add", "--quiet",
            &vendor_bare.display().to_string(),
            "vendor",
        ],
    );
    // A slot-local commit INSIDE the new submodule — its SHA never reaches the
    // declared origin (`vendor.git`). This pins that the populate sources from
    // the *slot's* clone: a naive `submodule update --init` from the declared
    // URL couldn't see this commit (and would hit the network), so it would
    // fail the gitlink-SHA assertion below.
    std::fs::write(slot_path.join("vendor").join("LOCAL"), b"slot-only\n").unwrap();
    git_commit(&slot_path.join("vendor"), "slot-local submodule commit");
    git_commit(&slot_path, "introduce vendor submodule");

    let rev = |repo: &Path| {
        String::from_utf8_lossy(
            &StdCommand::new("git")
                .args(["-C", &repo.display().to_string(), "rev-parse", "HEAD"])
                .output().unwrap().stdout,
        ).trim().to_string()
    };
    let slot_head = rev(&slot_path);
    let slot_sub_head = rev(&slot_path.join("vendor"));

    let out = session_cmd_cwd(&slot_path, &["land"]).output().unwrap();
    assert_ok(&out, "land");

    // `main` advanced to the slot's landed commit (no parallel land ⇒ pure ff).
    assert_eq!(rev(&source), slot_head, "main did not advance to the landed commit");

    // The newly-introduced submodule is populated in the main worktree, at the
    // pinned gitlink SHA.
    let main_sub = source.join("vendor");
    assert!(main_sub.join("LIB").exists(),
        "newly-introduced submodule's working tree did not materialize in main");
    assert!(main_sub.join("LOCAL").exists(),
        "the slot-local submodule commit did not reach main — populate sourced from origin, not the slot");
    assert_eq!(rev(&main_sub), slot_sub_head,
        "main's new submodule clone is not at the pinned gitlink SHA");

    // Origin points at the declared URL (survives slot recycle), not the
    // ephemeral slot clone the populate temporarily cloned from.
    let origin = String::from_utf8_lossy(
        &StdCommand::new("git")
            .args(["-C", &main_sub.display().to_string(), "remote", "get-url", "origin"])
            .output().unwrap().stdout,
    ).trim().to_string();
    assert_eq!(origin, vendor_bare.display().to_string(),
        "main's submodule origin should be the declared URL, not the slot clone");
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
    let slot_path = output_to_slot_path(&out);
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
