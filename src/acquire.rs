//! `acquire` orchestration. Picks an idle slot, pins HEAD to the requested commit,
//! writes the held-marker, renames the slot, creates a branch, runs submodule init.
//! See README.md §Lifecycle for the spec.
use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::cli::AcquireArgs;
use crate::config::PoolConfig;
use crate::{fs_paths, git, lock::Lock, mutex, slot, submodules};

pub fn run(pool_root: &Path, cfg: &PoolConfig, args: AcquireArgs) -> Result<()> {
    let group = slot::resolve_group(cfg, args.group.as_deref())?;
    let commitish = args
        .commit
        .as_deref()
        .unwrap_or(cfg.default_commit.as_str());
    let full_sha = git::resolve_full_sha(&cfg.source, commitish)?;

    // The "pick a slot + write the lock" critical section needs pool-wide serialization,
    // otherwise:
    //   (a) two parallel `--unique-sha` acquires for the same SHA both pass the scan
    //       and both write locks (race);
    //   (b) parallel acquires can both pick the same canonical N before either writes
    //       a lock to claim it.
    // Per-slot init mutex doesn't cover (b) since the slot-pick happens before mutex
    // acquisition. Use the pool-wide mutex for scan + pick + lock-write; release it
    // before the slow ops (`worktree add`, submodule init) that only need the per-slot mutex.
    let pool_mu = mutex::PoolMutex::acquire(fs_paths::pool_mutex(pool_root))
        .context("acquiring pool-wide mutex for slot allocation")?;

    if args.unique_sha
        && let Some(holder) = find_same_sha_holder(pool_root, cfg, &full_sha)?
    {
        bail!(
            "acquire --unique-sha failed: full_sha {} already held by slot '{}' (held since {}).\n\
             Wait for it, reuse its output, or run: worktree-pool --pool <key> release --name {}",
            &full_sha[..8],
            holder.name,
            holder.lock.started_at,
            holder.name
        );
    }

    // Capacity gate. Renamed slots hide their home N (no `slot_id` in lock) so we
    // can't infer budget from canonical-dir presence alone — count held-in-group locks.
    let held_in_group = slot::count_held_in_group(pool_root, cfg, group)?;
    if held_in_group >= cfg.max_slots as usize {
        let entries = slot::enumerate(pool_root, cfg)?;
        print_capacity_error(pool_root, cfg, group, &entries)?;
        bail!(
            "all {} {} slots are held",
            cfg.max_slots,
            group.unwrap_or("(no group)")
        );
    }

    // Iterate acquirable Ns (fresh + recycled-idle), try the init mutex on each.
    let entries = slot::enumerate(pool_root, cfg)?;
    let renamed_held: Vec<String> = entries
        .iter()
        .filter(|e| matches!(e.kind, slot::SlotEntryKind::Renamed))
        .map(|e| e.name.clone())
        .collect();
    let candidates = slot::acquirable_ns(pool_root, group, cfg.max_slots, &renamed_held)?;

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
             run `worktree-pool --pool <key> unstick` to clear stale ones"
        )
    })?;
    let canonical_path = pool_root.join(&slot_id);

    // Note: we keep the pool mutex through lock-write (below) but drop it before the slow
    // submodule init. That's why this whole block stays under `pool_mu`'s scope.
    let _ = &pool_mu;

    // Materialize the slot at full_sha. Fresh slot → `worktree add`; recycled → `reset --hard` + clean.
    let is_fresh = !canonical_path.join(".git").exists();
    if is_fresh {
        // Roll back partial state on failure: `worktree add` may leave a half-checked-out dir.
        if let Err(e) = git::worktree_add(&cfg.source, &canonical_path, &full_sha) {
            cleanup_partial_worktree_add(&cfg.source, &canonical_path);
            return Err(e);
        }
    } else {
        // Recycled slot: just `reset --hard`. Don't `git clean` — untracked files
        // in this slot are caller's warmth (Unity Library/, node_modules/, build outputs)
        // which the pool exists to preserve. The build system invalidates its own caches.
        git::reset_hard(&canonical_path, &full_sha)?;
    }

    // Resolve the worktree's gitdir. Stable across `git worktree move`.
    let gitdir = git::worktree_gitdir(&canonical_path)?;
    let lock_path = fs_paths::slot_lock(&gitdir);

    // Write the held marker BEFORE the rename. If we crash between rename and lock-write,
    // the slot would look idle (no lock) but live at a non-canonical name — undetectable.
    // Writing first means a crash leaves the slot held at its canonical id (clean recovery).
    let lock = Lock::new(full_sha.clone(), group.map(String::from));
    lock.write(&lock_path)
        .with_context(|| format!("writing lock {}", lock_path.display()))?;

    // Rename `{group}-N → <name>` and create the branch.
    let target_path = pool_root.join(&args.name);
    git::worktree_move(&cfg.source, &canonical_path, &target_path)
        .with_context(|| format!("moving {} → {}", canonical_path.display(), target_path.display()))?;
    git::checkout_force_branch(&target_path, &args.name)?;

    submodules::update(&target_path, cfg, &args.exclude_submodule_tags)?;

    println!("{}", target_path.display());
    Ok(())
}

struct Holder {
    name: String,
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
            return Ok(Some(Holder {
                name: entry.name,
                lock,
            }));
        }
    }
    Ok(None)
}

fn print_capacity_error(
    pool_root: &Path,
    _cfg: &PoolConfig,
    group: Option<&str>,
    entries: &[slot::SlotEntry],
) -> Result<()> {
    eprintln!("\nHeld slots in pool {}:", pool_root.display());
    for e in entries {
        if let slot::SlotEntryKind::Renamed = e.kind {
            eprintln!("  {}", e.name);
        }
    }
    let _ = group;
    Ok(())
}

fn cleanup_partial_worktree_add(source: &Path, slot: &Path) {
    // Best-effort rollback. Don't propagate errors; we're already in a failure path.
    let _ = git::worktree_remove(source, slot);
    let _ = std::fs::remove_dir_all(slot);
}

