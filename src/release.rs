//! `release` orchestration. Un-renames the slot to the smallest free `{group}-N`,
//! deletes the branch (local + remote best-effort), drops the lock.
//! See CLAUDE.md §Lifecycle for the spec.
//!
//! **Crash-safety invariant:** the lock file is removed LAST, after `worktree_rename`
//! completes. All earlier steps (detach, branch deletes, push delete, submodule
//! deletes) leave the slot semantically "held" — re-running release converges
//! because every step is idempotent. The post-rename + pre-lock-removal window
//! leaves a stale lock at the canonical-N gitdir; `reclaim_stale` (run at
//! every acquire/release startup) detects and removes it.
use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;

use crate::cli::ReleaseArgs;
use crate::config::PoolConfig;
use crate::{fs_paths, git, lock::Lock, mutex, slot, submodules};

/// Minimum age before a 0-byte `index.lock` is treated as a crashed-git leftover.
/// A live `git status` / `git commit` from inside a held slot momentarily owns
/// this file; 60s is well past any realistic git completion time.
const STALE_INDEX_LOCK_AFTER: Duration = Duration::from_secs(60);

pub fn run(pool_root: &Path, cfg: &PoolConfig, args: ReleaseArgs) -> Result<()> {
    // Pool-wide mutex serializes the find-smallest-free-N + rename window.
    let _pool_mu = mutex::PoolMutex::acquire(fs_paths::pool_mutex(pool_root))
        .context("acquiring pool mutex for release")?;

    if let Err(e) = reclaim_stale(pool_root, cfg) {
        eprintln!("warn: reclaim_stale during release: {e:#}");
    }

    let slot_path = pool_root.join(&args.name);
    if !slot_path.exists() {
        // Re-run on an already-released name — succeed silently for idempotency.
        eprintln!("release '{}': slot not present (already released)", args.name);
        return Ok(());
    }

    release_tail(pool_root, cfg, &slot_path, &args.name)
}

/// The release body without the pool mutex or the early-exit short-circuit.
/// Callable from `reclaim_stale`. Crash-safety invariant lives in the
/// module header above.
fn release_tail(
    pool_root: &Path,
    cfg: &PoolConfig,
    slot_path: &Path,
    name: &str,
) -> Result<()> {
    let gitdir = git::worktree_gitdir(slot_path)?;
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
            name
        );
        None
    };

    // Best-effort branch cleanup: detach, delete local branch, delete remote.
    // Order matters: can't delete the branch we're currently on; must detach first.
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

    // Mirror the parent cleanup into every submodule (incl. nested) — `acquire`
    // creates a `<name>` branch in each so commits there have a push-ready
    // label; release un-creates it. Best-effort, parallel per level.
    submodules::delete_branch_recursive(slot_path, name);

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
        .filter(|e| e.name != name)
        .filter(|e| matches!(e.kind, slot::SlotEntryKind::Renamed))
        .map(|e| e.name)
        .collect();

    let n = slot::smallest_free_n(pool_root, group, &held)?;
    let canonical = pool_root.join(slot::canonical_id(group, n));

    git::worktree_rename(&cfg.source, slot_path, &canonical).with_context(|| {
        format!(
            "moving {} → {} (un-rename on release)",
            slot_path.display(),
            canonical.display()
        )
    })?;

    // Lock removal LAST — the only step that flips held → idle on disk.
    // (Crash here leaves an orphan lock at canonical; `reclaim_stale` clears it.)
    match std::fs::remove_file(&lock_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("removing lock {}", lock_path.display()));
        }
    }

    eprintln!(
        "released '{}' → {}",
        name,
        canonical.file_name().and_then(|s| s.to_str()).unwrap_or("?")
    );
    Ok(())
}


/// Recovery sweep: detect and fix leftover state from crashed `acquire`/`release`.
/// Runs under the pool mutex at the start of every acquire/release so it never
/// races live mutators. Best-effort: failures log and continue.
/// See [[lifecycle.md#crash-recovery]] for the state table this implements.
///
/// **Provenance gate.** A `(Renamed, no lock)` slot is only treated as a legacy
/// zombie when the sticky `created` marker is present (proves it was once
/// pool-acquired). Without the marker — manual `git worktree add`, scratch dir,
/// or hand-deleted lock+marker — we leave it alone with a single warn. The
/// marker is co-written at acquire and never removed by release/reclaim/cleanup.
///
/// **Migration.** Pre-fix pools have lock files but no marker. Before classifying,
/// auto-stamp the marker on any slot whose lock is present (lock presence proves
/// pool ownership — only `acquire` writes locks). Pre-fix `(Renamed, no lock)`
/// zombies have lost provenance and must be cleared via `wt release --force`.
pub fn reclaim_stale(pool_root: &Path, cfg: &PoolConfig) -> Result<()> {
    let entries = slot::enumerate(pool_root, cfg)?;

    // Migration pass: stamp marker on any locked-but-unmarked slot. Idempotent.
    for entry in &entries {
        let Ok(gitdir) = git::worktree_gitdir(&entry.path) else { continue };
        let lock_path = fs_paths::slot_lock(&gitdir);
        let marker_path = fs_paths::slot_created_marker(&gitdir);
        if lock_path.exists() && !marker_path.exists() {
            eprintln!("reclaim_stale: stamping legacy pool marker on '{}'", entry.name);
            if let Err(e) = std::fs::write(
                &marker_path,
                "# worktree-pool slot provenance marker — do not delete\n",
            ) {
                eprintln!("warn: stamping marker {}: {e:#}", marker_path.display());
            }
        }
    }

    for entry in entries {
        let gitdir = match git::worktree_gitdir(&entry.path) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("reclaim_stale: '{}': worktree_gitdir failed ({e:#}); skipping", entry.name);
                continue;
            }
        };
        // Foreign-artifact sweep (git's index.lock); orthogonal to the table below.
        if clear_stale_index_lock(&gitdir) {
            eprintln!("reclaim_stale: cleared stale index.lock in '{}'", entry.name);
        }
        let lock_path = fs_paths::slot_lock(&gitdir);
        let marker_path = fs_paths::slot_created_marker(&gitdir);
        let lock_present = lock_path.exists();
        match (&entry.kind, lock_present) {
            (slot::SlotEntryKind::Renamed, false) => {
                if !marker_path.exists() {
                    eprintln!(
                        "reclaim_stale: '{}' (renamed dir, no lock, no pool marker) \
                         — not pool-owned; leaving untouched. Recover with `wt release --force {}` if intended.",
                        entry.name, entry.name
                    );
                    continue;
                }
                eprintln!(
                    "reclaim_stale: legacy zombie '{}' (renamed dir, no lock, marker present) — completing release",
                    entry.name
                );
                if let Err(e) = release_tail(pool_root, cfg, &entry.path, &entry.name) {
                    eprintln!("reclaim_stale: '{}' replay failed: {e:#}", entry.name);
                }
            }
            (slot::SlotEntryKind::Canonical { .. }, true) => {
                if is_post_release_orphan(&entry.path) {
                    eprintln!(
                        "reclaim_stale: orphan lock at '{}' (canonical, HEAD detached) — removing",
                        entry.name
                    );
                    if let Err(e) = std::fs::remove_file(&lock_path)
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        eprintln!("reclaim_stale: removing {}: {e:#}", lock_path.display());
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// True iff `<gitdir>/index.lock` matches the crashed-git-leftover signature
/// (0-byte file, mtime older than `STALE_INDEX_LOCK_AFTER`, not a symlink/dir).
/// See [[lifecycle.md#crash-recovery]] for the leak class + rationale.
///
/// Best-effort: I/O errors / future mtimes / non-file entries return `false`.
pub(crate) fn is_stale_index_lock(gitdir: &Path) -> bool {
    let Ok(md) = std::fs::symlink_metadata(fs_paths::worktree_index_lock(gitdir)) else {
        return false;
    };
    if !md.is_file() || md.len() != 0 {
        return false;
    }
    let Ok(mtime) = md.modified() else { return false };
    // Future mtime (clock skew / NTP jump) → treat as not stale yet.
    let Ok(age) = mtime.elapsed() else { return false };
    age >= STALE_INDEX_LOCK_AFTER
}

/// Remove a stale `index.lock` if present. Best-effort. Returns `true` iff
/// the file was actually unlinked (false if not stale, not present, or
/// removal raced/failed — failure logs).
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

/// True iff the slot's HEAD is not attached to a branch (detached, missing, or
/// otherwise unreadable by `symbolic-ref`). A live held slot is on a branch
/// (acquire's `checkout_force_branch`); a canonical-named dir whose HEAD is
/// off-branch with a lock present is unambiguously a post-release orphan or
/// an acquire that crashed before attaching the branch — both safe to reclaim.
fn is_post_release_orphan(slot: &Path) -> bool {
    match git::run_lenient(slot, &["symbolic-ref", "--quiet", "HEAD"]) {
        Ok((on_branch, _, _)) => !on_branch,
        Err(e) => {
            eprintln!("reclaim_stale: probing HEAD in {} failed ({e:#}); leaving lock alone",
                slot.display());
            false
        }
    }
}
