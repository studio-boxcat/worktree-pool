//! `acquire` orchestration. Picks an idle canonical slot, pins HEAD to the
//! requested commit, creates a branch (which flips idle → held).
//! See CLAUDE.md §Lifecycle for the spec.
use anyhow::{Context, Result};
use std::path::Path;

use crate::bail_exit;
use crate::cli::AcquireArgs;
use crate::config::PoolConfig;
use crate::exit::ExitKind;
use crate::types::{BranchName, FullSha, GroupName, SlotId};
use crate::{fs_paths, git, hooks, mutex, parallel, slot, submodules};

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

    // One enumeration feeds same-SHA scan, capacity check, and slot-pick.
    let entries = slot::enumerate(pool_root, cfg)?;

    if args.unique_sha
        && let Some(holder) = find_same_sha_holder(&entries, &full_sha)
    {
        bail_exit!(
            ExitKind::UniqueSha,
            "acquire --unique-sha failed: full_sha {} already held by slot '{}' (branch '{}').\n\
             Wait for it, reuse its output, or run: worktree-pool --pool <key> release {}",
            full_sha.short(),
            holder.slot_id,
            holder.branch,
            holder.branch
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

    // Materialize the slot at full_sha. Fresh → `worktree add`; recycled → `reset --hard`.
    if cand.is_fresh {
        if let Err(e) = git::worktree_add(&cfg.source, &canonical_path, full_sha.as_str()) {
            cleanup_partial_worktree_add(&cfg.source, &canonical_path);
            return Err(e);
        }
    } else {
        // Clear any leftover `index.lock` before `reset --hard`, which writes the
        // index and would fail with EEXIST on a stale lock (e.g. a crashed lazygit
        // in a prior session). The slot is idle (detached HEAD) and we hold the
        // pool + this slot's init mutex, so no legitimate git process is writing
        // the index — unconditional remove is race-free, and catches partial
        // non-zero locks a staleness heuristic would skip. git owns the lock's
        // lifecycle; we only sweep what a dead process left behind.
        if let Ok(gitdir) = git::worktree_gitdir(&canonical_path) {
            let lock = fs_paths::worktree_index_lock(&gitdir);
            match std::fs::remove_file(&lock) {
                Ok(()) => eprintln!("cleared leftover index.lock in '{slot_id}' before recycle"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // Don't swallow a real failure: the next reset --hard would hit
                // EEXIST with a generic git error, so surface the cause here.
                Err(e) => eprintln!("warn: removing {}: {e:#}", lock.display()),
            }
        }
        // Recycled slot: just `reset --hard`. Don't `git clean` — untracked files
        // (Unity Library/, node_modules/, build outputs) are caller warmth the pool
        // exists to preserve. The build system invalidates its own caches.
        git::reset_hard(&canonical_path, full_sha.as_str())?;
    }

    // Branch creation flips idle → held. If we crash between worktree-add/
    // reset and here, the slot stays detached (idle) — next acquire can safely
    // reclaim it. Pool mutex is still held, so no race.
    git::checkout_force_branch(&canonical_path, &args.name)?;

    // Slot is now visibly held; further state is per-slot only. Drop pool mutex
    // before submodule clone (potentially minutes for cold meow-tower).
    drop(pool_mu);

    submodules::update(&canonical_path, cfg, &args.exclude_submodule_tags, &args.name).with_context(|| {
        format!(
            "submodule update failed for slot '{slot_id}' (branch '{name}'); slot is left HELD with partial state. \
             Recover: `worktree-pool --pool <key> release {name}`.",
            name = args.name
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
        args.name.as_str(),
        cand.is_fresh,
    )
    .with_context(|| {
        format!(
            "post-acquire hook failed for slot '{slot_id}' (branch '{name}'); slot is left HELD. \
             Recover: `worktree-pool --pool <key> release {name}`.",
            name = args.name
        )
    })?;

    println!("{}", canonical_path.display());
    Ok(())
}

struct Holder {
    slot_id: SlotId,
    branch: BranchName,
}

fn find_same_sha_holder(entries: &[slot::SlotEntry], full_sha: &FullSha) -> Option<Holder> {
    // Parallel read: each entry needs a current_branch + rev-parse spawn.
    // Under the pool mutex, doing this serially blocks all other acquires;
    // the parallel version cuts ~5x on 8-slot pools.
    parallel::map(entries, |entry| {
        // `current_branch` returns None on a detached (idle) HEAD, so it doubles
        // as the held-gate — no separate `is_held_at` read needed.
        let branch = git::current_branch(&entry.path)?;
        let sha = git::run(&entry.path, &["rev-parse", "HEAD"]).ok()?;
        if sha != full_sha.as_str() {
            return None;
        }
        Some(Holder {
            slot_id: entry.id.clone(),
            branch: BranchName::from(branch),
        })
    })
    .into_iter()
    .flatten()
    .next()
}

fn print_capacity_error(pool_root: &Path, entries: &[slot::SlotEntry]) {
    eprintln!("\nHeld slots in pool {}:", pool_root.display());
    for e in entries {
        // Held iff HEAD is on a branch; idle (detached) slots have no name to list.
        if let Some(branch) = git::current_branch(&e.path) {
            eprintln!("  {} (branch: {})", e.id, branch);
        }
    }
}

// Best-effort rollback of a half-created worktree; the caller already propagates
// the real `worktree_add` error, so cleanup failures must not mask it.
fn cleanup_partial_worktree_add(source: &Path, slot: &Path) {
    let _ = git::worktree_remove(source, slot);
    let _ = std::fs::remove_dir_all(slot);
}
