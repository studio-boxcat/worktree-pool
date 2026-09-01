//! Submodule materialization, branching, tag-exclude, mirror gate, and recursion.
mod common;
use common::*;

use std::process::Command as StdCommand;

/// Nested submodules materialize end-to-end on acquire: outer + inner both
/// present, inner's HEAD attached to the slot's branch (the
/// `update_recursive` branch-attach convention applies to every depth).
#[test]
fn acquire_recurses_into_nested_submodules() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_nested_submodule(tmp.path());
    init_pool_mirror(&key, &bare, &tmp.path().join("staging"));

    let slot_name = "feat-nest";
    let out = acquire_dev_sub(&key, slot_name);
    assert_ok(&out, "nested acquire");
    let slot_path = output_to_slot_path(&out);

    let outer_file = slot_path.join("outer/OUTER_FILE");
    let inner_file = slot_path.join("outer/inner/FILE");
    assert!(outer_file.exists(), "outer materialization missing: {}", outer_file.display());
    assert!(inner_file.exists(), "nested inner materialization missing: {}", inner_file.display());

    // Branch-attach convention applies at every depth.
    let inner_path = slot_path.join("outer/inner");
    let head = StdCommand::new("git")
        .args(["symbolic-ref", "HEAD"])
        .current_dir(&inner_path)
        .output()
        .unwrap();
    assert!(head.status.success(), "nested submodule HEAD detached");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        format!("refs/heads/{slot_name}"),
        "nested submodule branch should match slot name"
    );

    // Release un-creates the nested branch (delete_branch_recursive).
    release(&key, slot_name);
    let canonical_inner = pool_root(&key).join("ios-0/outer/inner");
    let after = StdCommand::new("git")
        .args(["rev-parse", "--verify", &format!("refs/heads/{slot_name}")])
        .current_dir(&canonical_inner)
        .output()
        .unwrap();
    assert!(!after.status.success(),
        "nested submodule branch should be deleted on release; rev-parse returned: {}",
        String::from_utf8_lossy(&after.stdout));
}

/// `acquire --exclude-submodule-tags editor` skips the tagged submodule entirely:
/// its working dir stays empty (deinit'd) while the untagged one materializes.
#[test]
fn exclude_submodule_tags_skips_tagged_submodule() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_tagged_submodules(tmp.path());
    init_pool_mirror(&key, &bare, &tmp.path().join("staging"));

    let out = wtp()
        .args([
            "--pool", &key, "acquire", "--lease", "ci-1",
            "--group", "ios",
            "--exclude-submodule-tags", "editor",
        ])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap();
    assert_ok(&out, "exclude-tagged acquire");
    let slot = output_to_slot_path(&out);

    // Untagged submodule materialized.
    let runtime_file = slot.join("runtime_sub/FILE");
    assert!(runtime_file.exists(),
        "runtime_sub should be present: {}", runtime_file.display());

    // Tagged submodule's working dir is empty (deinit'd).
    let editor_file = slot.join("editor_sub/FILE");
    assert!(!editor_file.exists(),
        "editor_sub should be absent post-exclude; {} present",
        editor_file.display());
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
    init_pool_mirror(&key, &bare, &tmp.path().join("staging"));

    let slot_name = "feat-branch";
    let out = acquire_dev_sub(&key, slot_name);
    assert!(
        out.status.success(),
        "acquire failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let slot = output_to_slot_path(&out);
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
    init_pool_mirror(&key, &bare, &tmp.path().join("staging"));

    let out = wtp()
        .args(["--pool", &key, "acquire", "--lease", "many", "--group", "ios"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "acquire failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let slot = output_to_slot_path(&out);

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

/// Release cleans up submodule branches — acquire creates a `<name>` branch in
/// each submodule, and release's `delete_branch_recursive` removes them.
#[test]
fn release_replay_completes_with_submodules() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());
    init_pool_mirror(&key, &bare, &tmp.path().join("staging"));

    let out = acquire_dev(&key, "feat-sub");
    assert_ok(&out, "");
    let slot = output_to_slot_path(&out);

    // Submodule is initialized; acquire created a `feat-sub` branch in it.
    let sub = slot.join("sub");
    assert!(sub.join(".git").exists(), "submodule should be initialized");
    let sub_branches_before = run_git_capture(&sub, &["branch", "--list", "feat-sub"]);
    assert!(sub_branches_before.contains("feat-sub"),
        "submodule should have feat-sub branch: {sub_branches_before:?}");

    // Slot is held (on branch feat-sub).
    let head = run_git_capture(&slot, &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(head.trim(), "feat-sub");

    // Release detaches HEAD + cleans up branches in parent and submodules.
    release(&key, "feat-sub");

    // Slot is now idle (detached HEAD).
    assert_head_detached(&slot);

    // Submodule's per-slot branch should be gone too.
    let sub_branches_after = run_git_capture(&sub, &["branch", "--list", "feat-sub"]);
    assert!(sub_branches_after.trim().is_empty(),
        "submodule's feat-sub branch should be cleaned: {sub_branches_after:?}");
}

/// `init` refuses a submodule-bearing source when no mirror mode is given — the
/// silent declared-URL fallback is gone, so the operator must choose. No pool
/// config is written on the rejected init.
#[test]
fn init_refuses_submodule_source_without_mirror() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());

    let out = wtp()
        .args(["--pool", &key, "init"])
        .arg("--source")
        .arg(&bare)
        .args(["--max-slots", "4", "--groups", "ios,android"])
        .output()
        .unwrap();

    assert!(!out.status.success(), "init must refuse a no-mirror submodule pool");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("submodule mirror"),
        "stderr should name the mirror requirement, got: {stderr}"
    );
    assert!(
        !pool_root(&key).join(".meta/config.yaml").exists(),
        "rejected init must not leave a pool config behind"
    );
}

/// `init` with an explicit `source-submodules` mirror accepts a submodule-bearing
/// source: the operator named a local mirror, so there's no silent network path.
#[test]
fn init_accepts_submodule_source_with_git_modules_mirror() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());
    init_pool_mirror(&key, &bare, &tmp.path().join("staging")); // asserts success
    assert!(pool_root(&key).join(".meta/config.yaml").exists());
}

/// A submodule-less source needs no mirror — `init` without mirror flags still
/// succeeds (the gate keys on submodule presence, not on the flag).
#[test]
fn init_allows_submoduleless_source_without_mirror() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture(tmp.path());
    init_pool(&key, &bare); // asserts success
}

/// Acquire backstop: a pool whose config carries no mirror but whose source has
/// submodules (e.g. created before the gate existed) must fail BEFORE the
/// idle→held flip, leaving the slot detached (idle) and cleanly reclaimable
/// rather than HELD with a half-fetched submodule tree.
#[test]
fn acquire_backstop_refuses_no_mirror_submodule_pool_and_keeps_slot_idle() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());

    // Hand-write a mirror-less config, bypassing the init gate.
    let pool = pool_root(&key);
    std::fs::create_dir_all(pool.join(".meta")).unwrap();
    std::fs::write(
        pool.join(".meta/config.yaml"),
        format!(
            "schema_version: 1\nsource: {}\ndefault_commit: main\nmax_slots: 4\ngroups: ios,android\n",
            bare.display()
        ),
    )
    .unwrap();

    let out = acquire_dev_sub(&key, "feat");
    assert!(!out.status.success(), "acquire must refuse a no-mirror submodule pool");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("submodule mirror"),
        "stderr should explain the missing mirror, got: {stderr}"
    );

    // Slot was materialized then bailed pre-held → detached HEAD (idle), not on a branch.
    let slot = pool.join("ios-0");
    let head = StdCommand::new("git")
        .current_dir(&slot)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .unwrap();
    assert!(
        !head.status.success(),
        "backstop must leave the slot detached (idle), but HEAD is on a branch"
    );
}

/// A submodule dropped from `.gitmodules` between two acquires leaves its working
/// dir behind (`reset --hard` never touches untracked paths); recycle must sweep
/// it — a checkout consumer can't tell the stale copy from a declared one — while
/// ordinary untracked warmth survives.
#[test]
fn recycled_acquire_sweeps_stranded_submodule_dir() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_submodule(tmp.path());
    let staging = tmp.path().join("staging");
    init_pool_mirror(&key, &bare, &staging);

    let out = acquire_dev_sub(&key, "first");
    assert_ok(&out, "initial acquire");
    let slot = output_to_slot_path(&out);
    assert!(slot.join("sub/FILE").exists(), "submodule should materialize");
    std::fs::write(slot.join("warm.txt"), b"warm").unwrap();
    std::fs::create_dir_all(slot.join("warm-dir")).unwrap();
    std::fs::write(slot.join("warm-dir/f"), b"warm").unwrap();
    release(&key, "first");

    // Drop the submodule at the tip (`git rm` removes gitlink + .gitmodules entry).
    run_git(&staging, &["rm", "--quiet", "sub"]);
    git_commit(&staging, "drop submodule");
    run_git(&staging, &["push", "--quiet", "origin", "main"]);

    let out = acquire_dev_sub(&key, "second");
    assert_ok(&out, "recycled acquire");
    assert_eq!(output_to_slot_path(&out), slot, "should recycle the same canonical slot");
    assert!(!slot.join("sub").exists(), "stranded submodule working dir must be swept");
    assert!(slot.join("warm.txt").exists(), "untracked warmth file must survive");
    assert!(slot.join("warm-dir/f").exists(), "untracked non-repo dir must survive");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("swept stranded git working dir"),
        "sweep should leave a breadcrumb, got: {stderr}"
    );
}

/// Same stranding one level down: a nested submodule dropped from the outer's
/// `.gitmodules` strands inside the outer's working tree, where the parent-level
/// sweep can't see it — the per-submodule sweep in `update_recursive` must.
#[test]
fn recycled_acquire_sweeps_stranded_nested_submodule_dir() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    let bare = make_fixture_with_nested_submodule(tmp.path());
    let staging = tmp.path().join("staging");
    init_pool_mirror(&key, &bare, &staging);

    let out = acquire_dev_sub(&key, "first");
    assert_ok(&out, "initial acquire");
    let slot = output_to_slot_path(&out);
    assert!(slot.join("outer/inner/FILE").exists(), "nested submodule should materialize");
    release(&key, "first");

    // Drop `inner` at outer's tip, then bump outer's pin in the superproject.
    // Pulling inside staging's own `outer/` both advances the pin source and
    // lands the new commit in `staging/.git/modules/outer` — the object store
    // the pool's source-submodules mirror serves slots from.
    let outer_staging = tmp.path().join("outer-staging");
    run_git(&outer_staging, &["rm", "--quiet", "inner"]);
    git_commit(&outer_staging, "drop inner");
    run_git(&outer_staging, &["push", "--quiet", "origin", "main"]);
    run_git(&staging.join("outer"), &["pull", "--quiet", "origin", "main"]);
    git_commit(&staging, "bump outer past the drop");
    run_git(&staging, &["push", "--quiet", "origin", "main"]);

    let out = acquire_dev_sub(&key, "second");
    assert_ok(&out, "recycled acquire");
    assert_eq!(output_to_slot_path(&out), slot, "should recycle the same canonical slot");
    assert!(
        !slot.join("outer/inner").exists(),
        "stranded nested submodule working dir must be swept"
    );
    assert!(slot.join("outer/OUTER_FILE").exists(), "outer itself must stay materialized");
}

/// Capstone for the original incident: a working-clone source whose submodule is
/// advanced to a LOCAL-ONLY commit (present in `<source>/.git/modules/<name>` but
/// never pushed to the submodule's origin). `source-submodules` mirror resolves it from
/// the source's own object store, so acquire succeeds where a network fetch would
/// fail with `not our ref`.
#[test]
fn acquire_resolves_local_only_submodule_via_git_modules_mirror() {
    let key = pool_key();
    let _c = Cleanup(key.clone());
    let tmp = tempfile::TempDir::new().unwrap();
    make_fixture_with_submodule(tmp.path());
    let source = tmp.path().join("staging"); // working clone with `sub/` inited

    // Advance the submodule to a commit that is NOT pushed to its origin (sub.git).
    let sub = source.join("sub");
    std::fs::write(sub.join("LOCAL"), b"local-only change").unwrap();
    git_commit(&sub, "local-only submodule commit");
    let local_sha = String::from_utf8_lossy(
        &StdCommand::new("git")
            .current_dir(&sub)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    // Bump the gitlink in the superproject and commit (advances staging's main).
    git_commit(&source, "bump submodule to local-only commit");

    // Init with source-submodules mirror rooted at the source itself.
    wtp()
        .args(["--pool", &key, "init"])
        .arg("--source")
        .arg(&source)
        .args([
            "--max-slots", "4", "--groups", "ios,android",
            "--submodule-mirror-mode", "source-submodules", "--submodule-mirror-base",
        ])
        .arg(&source)
        .assert()
        .success();

    let out = acquire_dev_sub(&key, "feat");
    assert_ok(&out, "acquire should resolve the local-only submodule via the mirror");
    let slot = output_to_slot_path(&out);

    let sub_head = String::from_utf8_lossy(
        &StdCommand::new("git")
            .current_dir(slot.join("sub"))
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(
        sub_head, local_sha,
        "slot submodule should be checked out at the local-only commit"
    );
}
