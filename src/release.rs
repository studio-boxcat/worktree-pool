//! `release` orchestration. Drops the held marker, deletes the branch.
//! Slot dir stays at its canonical path; no rename.
//! See CLAUDE.md §Lifecycle for the spec.
//!
//! **Crash-safety invariant:** the lock file is removed LAST. All earlier
//! steps (detach, branch deletes, push delete, submodule deletes) leave the
//! slot semantically "held" — re-running release converges because every step
//! is idempotent.
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

use crate::cli::ReleaseArgs;
use crate::config::PoolConfig;
use crate::{fs_paths, git, mutex, slot, submodules};

/// Minimum age before a 0-byte `index.lock` is treated as a crashed-git leftover.
/// A live `git status` / `git commit` from inside a held slot momentarily owns
/// this file; 60s is well past any realistic git completion time.
const STALE_INDEX_LOCK_AFTER: Duration = Duration::from_secs(60);

pub fn run(pool_root: &Path, cfg: &PoolConfig, args: ReleaseArgs) -> Result<()> {
    let _pool_mu = mutex::PoolMutex::acquire(fs_paths::pool_mutex(pool_root))
        .context("acquiring pool mutex for release")?;

    if let Err(e) = reclaim_stale(pool_root, cfg) {
        eprintln!("warn: reclaim_stale during release: {e:#}");
    }

    // Lookup order: branch name (normal), then canonical slot id (operator
    // recovering a detached-HEAD slot whose branch was hand-deleted, or any
    // held slot the operator addresses by its on-disk id).
    if let Some(entry) = slot::find_by_name(pool_root, cfg, &args.name)? {
        return release_tail(&entry.path, &args.name);
    }
    let path = pool_root.join(&args.name);
    if path.is_dir()
        && let Ok(gitdir) = git::worktree_gitdir(&path)
        && fs_paths::slot_lock(&gitdir).exists()
    {
        let name = git::current_branch(&path).unwrap_or_else(|| args.name.clone());
        return release_tail(&path, &name);
    }
    eprintln!("release '{}': no held slot (already released)", args.name);
    Ok(())
}

/// The release body without the pool mutex or the find-by-name lookup.
/// Idempotent — see crash-safety invariant in the module header.
fn release_tail(slot_path: &Path, name: &str) -> Result<()> {
    let gitdir = git::worktree_gitdir(slot_path)?;
    let lock_path = fs_paths::slot_lock(&gitdir);

    // Best-effort branch cleanup: detach, delete local branch, delete remote.
    // Order matters: can't delete the branch we're on; detach first.
    let (detach_ok, _, detach_err) = git::detach_head(slot_path)?;
    if !detach_ok {
        eprintln!(
            "warn: detach HEAD failed in {}; branch '{}' may persist as a dangling ref. {}",
            slot_path.display(),
            name,
            detach_err
        );
    }
    let _ = git::branch_delete(slot_path, name);
    let _ = git::push_delete(slot_path, "origin", name);

    // Mirror parent cleanup into every submodule (incl. nested) — acquire created
    // a `<name>` branch in each so commits have a push-ready label; release un-creates.
    submodules::delete_branch_recursive(slot_path, name);

    // Lock removal LAST — the only step that flips held → idle on disk.
    match std::fs::remove_file(&lock_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("removing lock {}", lock_path.display()));
        }
    }

    eprintln!("released '{}'", name);
    Ok(())
}

/// Recovery sweep: clears foreign git artifacts (`index.lock`) left over from
/// SIGKILL'd git processes inside slot worktrees. Runs under the pool mutex at
/// the start of every acquire/release.
///
/// **Does NOT auto-replay crashed acquire/release.** Without a per-slot
/// "in-flight" signal that survives normal process exit (init-mutex flock is
/// auto-released on any exit, so it can't disambiguate completed from
/// crashed), we can't safely tell "crashed mid-flight" from "completed and
/// exited". Operators release stuck slots manually: `release NAME` (by
/// branch) or `release <slot-id>` (canonical-id fallback for detached-HEAD
/// slots from a crashed mid-acquire). Release is idempotent so re-running is
/// safe.
pub fn reclaim_stale(pool_root: &Path, cfg: &PoolConfig) -> Result<()> {
    for entry in slot::enumerate(pool_root, cfg)? {
        let Ok(gitdir) = git::worktree_gitdir(&entry.path) else {
            continue;
        };
        if clear_stale_index_lock(&gitdir) {
            eprintln!("reclaim_stale: cleared stale index.lock in '{}'", entry.id);
        }
    }
    Ok(())
}

/// True iff `<gitdir>/index.lock` matches the crashed-git-leftover signature
/// (0-byte file, mtime older than `STALE_INDEX_LOCK_AFTER`, not symlink/dir).
pub(crate) fn is_stale_index_lock(gitdir: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(fs_paths::worktree_index_lock(gitdir)) else {
        return false;
    };
    if !md.is_file() || md.len() != 0 {
        return false;
    }
    let Ok(mtime) = md.modified() else { return false };
    let Ok(age) = mtime.elapsed() else { return false };
    age >= STALE_INDEX_LOCK_AFTER
}

fn clear_stale_index_lock(gitdir: &Path) -> bool {
    if !is_stale_index_lock(gitdir) {
        return false;
    }
    let lock_path = fs_paths::worktree_index_lock(gitdir);
    match std::fs::remove_file(&lock_path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            eprintln!("warn: removing stale {}: {e:#}", lock_path.display());
            false
        }
    }
}

