//! `acquire` orchestration. Picks an idle canonical slot, pins HEAD to the
//! requested commit, creates a branch (which flips idle → held).
//! See CLAUDE.md §Lifecycle for the spec.
use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::bail_exit;
use crate::cli::AcquireArgs;
use crate::config::PoolConfig;
use crate::exit::ExitKind;
use crate::types::{GroupName, LeaseName, SlotId};
use crate::{fs_paths, git, hooks, mutex, slot, submodules};

pub fn run(pool_key: &str, pool_root: &Path, cfg: &PoolConfig, args: AcquireArgs) -> Result<()> {
    let group = slot::resolve_group(cfg, args.group.as_deref())?;
    let commitish = args
        .commit
        .as_deref()
        .unwrap_or(cfg.default_commit.as_str());
    let full_sha = git::resolve_full_sha(&cfg.source, commitish)?;

    // Pool-wide mutex covers slot allocation + branch creation (the idle→held
    // transition). See module-level serialization rationale in earlier history.
    let pool_mu = mutex::FileLock::acquire(fs_paths::pool_mutex(pool_root))
        .context("acquiring pool-wide mutex for slot allocation")?;

    // One enumeration feeds the capacity check and slot-pick.
    let entries = slot::enumerate(pool_root, cfg)?;

    // Refuse here because nothing downstream would: `checkout_force_branch` bypasses git's
    // "branch already checked out elsewhere" guard, and `find_by_lease` resolves duplicates in
    // `read_dir` order — so `release` could detach the wrong slot under a live consumer.
    // What a lease means, and why this is the only duplicate-work guard: [[lifecycle.md#identity-model]].
    let lease = LeaseName::from(args.lease.clone());
    if let Some(entry) = slot::find_by_lease(pool_root, cfg, &lease)? {
        bail_exit!(
            ExitKind::LeaseHeld,
            "acquire failed: lease '{}' is already held by slot '{}'.\n\
             Pick a different lease, or run: worktree-pool --pool <key> release --lease {}",
            args.lease,
            entry.id,
            args.lease
        );
    }

    let occupying = slot::count_held_in_group(&entries, group);
    if occupying >= cfg.max_slots as usize {
        print_capacity_error(pool_root, &entries);
        bail_exit!(
            ExitKind::Capacity,
            "all {} {} slots are held",
            cfg.max_slots,
            group.map(GroupName::as_str).unwrap_or("(no group)")
        );
    }

    let candidates = slot::acquirable(pool_root, group, cfg.max_slots, &entries);

    let mut acquired: Option<(slot::Acquirable, SlotId, mutex::FileLock)> = None;
    for cand in candidates {
        let id = slot::canonical_id(group, cand.n);
        let mutex_path = fs_paths::init_mutex(pool_root, id.as_str());
        if let Some(m) = mutex::FileLock::try_acquire(mutex_path)? {
            acquired = Some((cand, id, m));
            break;
        }
    }
    let Some((cand, slot_id, _mutex)) = acquired else {
        bail_exit!(
            ExitKind::Contended,
            "all candidate slots have init mutexes held by other acquires; \
             run `worktree-pool --pool <key> unstick` to inspect"
        );
    };
    let canonical_path = pool_root.join(slot_id.as_str());

    // Materialize the slot at full_sha. Fresh → `worktree add`; recycled → `recycle_slot`.
    if cand.is_fresh {
        if let Err(e) = git::worktree_add(&cfg.source, &canonical_path, full_sha.as_str()) {
            cleanup_partial_worktree_add(&cfg.source, &canonical_path);
            return Err(e);
        }
    } else {
        recycle_slot(&canonical_path, &slot_id, full_sha.as_str())?;
    }

    // Backstop the init-time mirror gate BEFORE the idle→held flip: a pool
    // created before the gate (or whose source gained submodules since) could
    // still carry submodules with no mirror configured. Bailing here leaves the
    // slot detached (idle) and cleanly reclaimable, rather than HELD with a
    // half-fetched submodule tree. See [[docs/lifecycle.md]].
    if cfg.submodule_mirror_mode.is_none()
        && submodules::slot_declares_submodules(&canonical_path)?
    {
        bail!(
            "slot '{slot_id}' checkout declares submodules but pool has no submodule mirror \
             configured. Set submodule_mirror_mode (source-submodules or bare-mirror) + \
             submodule_mirror_base in {}, then retry.",
            fs_paths::pool_config(pool_root).display()
        );
    }

    // Branch creation flips idle → held. If we crash between worktree-add/
    // reset and here, the slot stays detached (idle) — next acquire can safely
    // reclaim it. Pool mutex is still held, so no race.
    git::checkout_force_branch(&canonical_path, &args.lease)?;

    // Slot is now visibly held; further state is per-slot only. Drop pool mutex
    // before submodule clone (potentially minutes for cold meow-tower).
    drop(pool_mu);

    submodules::update(&canonical_path, cfg, &args.exclude_submodule_tags, &args.lease).with_context(|| {
        format!(
            "submodule update failed for slot '{slot_id}' (lease '{lease}'); slot is left HELD with partial state. \
             Recover: `worktree-pool --pool <key> release --lease {lease}`.",
            lease = args.lease
        )
    })?;

    // Post-acquire hook (if the slot's checkout ships `.wt-hooks.sh`). Runs in
    // the slot with the WT_* contract; fires for direct `worktree-pool acquire`
    // (build pools) as well as `wt go`. Fail-loud + BEFORE the path is printed,
    // so a rejecting hook — e.g. langpack `ensure` on a stale release pin —
    // fails the acquire rather than yielding a usable slot. On failure the slot
    // is already HELD (branch created above), so — mirroring the submodule path
    // — we leave it HELD and hand the caller a release breadcrumb rather than
    // silently leaking the slot.
    hooks::fire(
        "wt_post_acquire",
        &canonical_path,
        pool_key,
        args.lease.as_str(),
        cand.is_fresh,
    )
    .with_context(|| {
        format!(
            "post-acquire hook failed for slot '{slot_id}' (branch '{name}'); slot is left HELD. \
             Recover: `worktree-pool --pool <key> release {name}`.",
            name = args.lease
        )
    })?;

    println!("{}", canonical_path.display());
    Ok(())
}

/// Re-pin an idle slot to `full_sha` in place. No `git clean` — untracked files
/// (Unity `Library/`, `node_modules/`, build outputs) are caller warmth the pool
/// exists to preserve; the build system invalidates its own caches. The one
/// exception is `sweep_stranded`: an untracked dir that is itself a git repo is
/// topology-change litter (a dropped submodule's leftover working dir), not warmth.
fn recycle_slot(canonical_path: &Path, slot_id: &SlotId, full_sha: &str) -> Result<()> {
    // Clear any leftover `index.lock` before `reset --hard`, which writes the
    // index and would fail with EEXIST on a stale lock (e.g. a crashed lazygit
    // in a prior session). The slot is idle (detached HEAD) and we hold the
    // pool + this slot's init mutex, so no legitimate git process is writing
    // the index — unconditional remove is race-free, and catches partial
    // non-zero locks a staleness heuristic would skip. git owns the lock's
    // lifecycle; we only sweep what a dead process left behind.
    if let Ok(gitdir) = git::worktree_gitdir(canonical_path) {
        let lock = fs_paths::worktree_index_lock(&gitdir);
        match std::fs::remove_file(&lock) {
            Ok(()) => eprintln!("cleared leftover index.lock in '{slot_id}' before recycle"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Don't swallow a real failure: the next reset --hard would hit
            // EEXIST with a generic git error, so surface the cause here.
            Err(e) => eprintln!("warn: removing {}: {e:#}", lock.display()),
        }
    }
    git::reset_hard(canonical_path, full_sha)?;
    submodules::sweep_stranded(canonical_path)
}

fn print_capacity_error(pool_root: &Path, entries: &[slot::SlotEntry]) {
    eprintln!("\nHeld slots in pool {}:", pool_root.display());
    for e in entries {
        // Held iff HEAD is on a branch; idle (detached) slots hold no lease to list.
        if let Some(lease) = git::current_branch(&e.path) {
            eprintln!("  {} (lease: {})", e.id, lease);
        }
    }
}

// Best-effort rollback of a half-created worktree; the caller already propagates
// the real `worktree_add` error, so cleanup failures must not mask it.
fn cleanup_partial_worktree_add(source: &Path, slot: &Path) {
    let _ = git::worktree_remove(source, slot);
    let _ = std::fs::remove_dir_all(slot);
}
