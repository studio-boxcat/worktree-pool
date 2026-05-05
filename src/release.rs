//! `release` orchestration. Un-renames the slot to the smallest free `{group}-N`,
//! deletes the branch (local + remote best-effort), drops the lock.
//! See README.md §Lifecycle for the spec.
use anyhow::{Context, Result};
use std::path::Path;

use crate::cli::ReleaseArgs;
use crate::config::PoolConfig;
use crate::{fs_paths, git, lock::Lock, mutex, slot};

pub fn run(pool_root: &Path, cfg: &PoolConfig, args: ReleaseArgs) -> Result<()> {
    // Pool-wide mutex serializes the find-smallest-free-N + rename window.
    let _pool_mu = mutex::PoolMutex::acquire(fs_paths::pool_mutex(pool_root))
        .context("acquiring pool mutex for release")?;

    let slot_path = pool_root.join(&args.name);
    if !slot_path.exists() {
        // Idempotent: re-running on an already-released name succeeds silently.
        eprintln!("release '{}': slot not present (already released)", args.name);
        return Ok(());
    }

    // Read the lock to recover the slot's group (for un-rename namespace).
    let gitdir = git::worktree_gitdir(&slot_path)?;
    let lock_path = fs_paths::slot_lock(&gitdir);
    let group_from_lock: Option<String> = if lock_path.exists() {
        match Lock::read(&lock_path) {
            Ok(l) => l.group,
            Err(e) => {
                eprintln!(
                    "warn: lock at {} unparseable ({:#}); proceeding with release",
                    lock_path.display(),
                    e
                );
                None
            }
        }
    } else {
        eprintln!(
            "warn: no lock at {} for slot '{}'; releasing anyway",
            lock_path.display(),
            args.name
        );
        None
    };

    // Drop the lock first. After this point, the slot is "idle" from another acquire's
    // perspective — but we still hold the pool-wide release mutex, so no one is scanning.
    if lock_path.exists() {
        std::fs::remove_file(&lock_path)
            .with_context(|| format!("removing lock {}", lock_path.display()))?;
    }

    // Best-effort branch cleanup: detach, delete local branch, delete remote.
    // Order matters: can't delete the branch we're currently on; must detach first.
    let (detach_ok, _, detach_err) = git::checkout_detach(&slot_path)?;
    if !detach_ok {
        eprintln!(
            "warn: 'git checkout --detach' failed in {}; branch '{}' may persist as a dangling ref. {}",
            slot_path.display(),
            args.name,
            detach_err
        );
    }
    let _ = git::branch_delete(&slot_path, &args.name);
    let _ = git::push_delete(&slot_path, "origin", &args.name);

    // Compute target canonical id. Group must come from the lock (acquire wrote it there);
    // if absent, fall back to first configured group, or groupless.
    let group: Option<&str> = match group_from_lock.as_deref() {
        Some(g) if cfg.groups.iter().any(|x| x == g) => Some(g),
        _ if cfg.groups.is_empty() => None,
        _ => Some(cfg.groups[0].as_str()),
    };

    // Build the held-names list for `smallest_free_n` — names of currently-renamed slots
    // EXCLUDING the one we're releasing (which is about to vanish).
    let held: Vec<String> = slot::enumerate(pool_root, cfg)?
        .into_iter()
        .filter(|e| e.name != args.name)
        .filter(|e| matches!(e.kind, slot::SlotEntryKind::Renamed))
        .map(|e| e.name)
        .collect();

    let n = slot::smallest_free_n(pool_root, group, cfg.max_slots, &held)?;
    let canonical = pool_root.join(slot::canonical_id(group, n));

    git::worktree_move(&cfg.source, &slot_path, &canonical).with_context(|| {
        format!(
            "moving {} → {} (un-rename on release)",
            slot_path.display(),
            canonical.display()
        )
    })?;

    eprintln!(
        "released '{}' → {}",
        args.name,
        canonical.file_name().and_then(|s| s.to_str()).unwrap_or("?")
    );
    Ok(())
}
