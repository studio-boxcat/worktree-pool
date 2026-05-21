//! `acquire` orchestration. Picks an idle canonical slot, pins HEAD to the
//! requested commit, writes the held-marker, creates a branch.
//! See CLAUDE.md §Lifecycle for the spec.
use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::cli::AcquireArgs;
use crate::config::PoolConfig;
use crate::{fs_paths, git, lock::Lock, mutex, release, slot, submodules};

pub fn run(pool_root: &Path, cfg: &PoolConfig, args: AcquireArgs) -> Result<()> {
    let group = slot::resolve_group(cfg, args.group.as_deref())?;
    let commitish = args
        .commit
        .as_deref()
        .unwrap_or(cfg.default_commit.as_str());
    let full_sha = git::resolve_full_sha(&cfg.source, commitish)?;

    // Pool-wide mutex covers slot allocation + lock-write. See module-level
    // serialization rationale (race classes a/b) in earlier history.
    let pool_mu = mutex::PoolMutex::acquire(fs_paths::pool_mutex(pool_root))
        .context("acquiring pool-wide mutex for slot allocation")?;

    if let Err(e) = release::reclaim_stale(pool_root, cfg) {
        eprintln!("warn: reclaim_stale during acquire: {e:#}");
    }

    if args.unique_sha
        && let Some(holder) = find_same_sha_holder(pool_root, cfg, &full_sha)?
    {
        bail!(
            "acquire --unique-sha failed: full_sha {} already held by slot '{}' (branch '{}', held since {}).\n\
             Wait for it, reuse its output, or run: worktree-pool --pool <key> release {}",
            &full_sha[..8],
            holder.slot_id,
            holder.branch,
            holder.lock.started_at,
            holder.branch
        );
    }

    let occupying = slot::count_held_in_group(pool_root, cfg, group)?;
    if occupying >= cfg.max_slots as usize {
        let entries = slot::enumerate(pool_root, cfg)?;
        print_capacity_error(pool_root, &entries)?;
        bail!(
            "all {} {} slots are held",
            cfg.max_slots,
            group.unwrap_or("(no group)")
        );
    }

    let entries = slot::enumerate(pool_root, cfg)?;
    let candidates = slot::acquirable_ns(pool_root, group, cfg.max_slots, &entries)?;

    let mut acquired_mutex: Option<(String, mutex::InitMutex)> = None;
    for n in candidates {
        let id = slot::canonical_id(group, n);
        let mutex_path = fs_paths::init_mutex(pool_root, &id);
        match mutex::InitMutex::try_acquire(mutex_path)? {
            Some(m) => {
                acquired_mutex = Some((id, m));
                break;
            }
            None => continue,
        }
    }
    let (slot_id, _mutex) = acquired_mutex.ok_or_else(|| {
        anyhow::anyhow!(
            "all candidate slots have init mutexes held by other acquires; \
             run `worktree-pool --pool <key> unstick` to inspect"
        )
    })?;
    let canonical_path = pool_root.join(&slot_id);

    // Materialize the slot at full_sha. Fresh → `worktree add`; recycled → `reset --hard`.
    let is_fresh = !canonical_path.join(".git").exists();
    if is_fresh {
        if let Err(e) = git::worktree_add(&cfg.source, &canonical_path, &full_sha) {
            cleanup_partial_worktree_add(&cfg.source, &canonical_path);
            return Err(e);
        }
    } else {
        // Recycled slot: just `reset --hard`. Don't `git clean` — untracked files
        // (Unity Library/, node_modules/, build outputs) are caller warmth the pool
        // exists to preserve. The build system invalidates its own caches.
        git::reset_hard(&canonical_path, &full_sha)?;
    }

    let gitdir = git::worktree_gitdir(&canonical_path)?;
    let lock_path = fs_paths::slot_lock(&gitdir);

    // Lock written BEFORE branch checkout. If we crash between, the slot is
    // left held with detached HEAD; operator recovers via `release <slot-id>`
    // (canonical-id fallback path in release::run).
    let lock = Lock::new(full_sha.clone(), group.map(String::from));
    lock.write(&lock_path)
        .with_context(|| format!("writing lock {}", lock_path.display()))?;

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

    println!("{}", canonical_path.display());
    Ok(())
}

struct Holder {
    slot_id: String,
    branch: String,
    lock: Lock,
}

fn find_same_sha_holder(
    pool_root: &Path,
    cfg: &PoolConfig,
    full_sha: &str,
) -> Result<Option<Holder>> {
    for entry in slot::enumerate(pool_root, cfg)? {
        let Ok(gitdir) = git::worktree_gitdir(&entry.path) else {
            continue;
        };
        let lock_path = fs_paths::slot_lock(&gitdir);
        if !lock_path.exists() {
            continue;
        }
        let Ok(lock) = Lock::read(&lock_path) else {
            continue;
        };
        if lock.full_sha == full_sha {
            let branch = git::current_branch(&entry.path).unwrap_or_else(|| "(detached)".into());
            return Ok(Some(Holder {
                slot_id: entry.id,
                branch,
                lock,
            }));
        }
    }
    Ok(None)
}

fn print_capacity_error(pool_root: &Path, entries: &[slot::SlotEntry]) -> Result<()> {
    eprintln!("\nHeld slots in pool {}:", pool_root.display());
    for e in entries {
        if !slot::is_held_at(&e.path) {
            continue;
        }
        let branch = git::current_branch(&e.path).unwrap_or_else(|| "(detached)".into());
        eprintln!("  {} (branch: {})", e.id, branch);
    }
    Ok(())
}

fn cleanup_partial_worktree_add(source: &Path, slot: &Path) {
    let _ = git::worktree_remove(source, slot);
    let _ = std::fs::remove_dir_all(slot);
}
