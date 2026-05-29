//! `release` orchestration. Detaches HEAD (held → idle), deletes the branch.
//! Slot dir stays at its canonical path; no rename.
//! See CLAUDE.md §Lifecycle for the spec.
//!
//! Under the pool mutex, detach HEAD flips held → idle. Subsequent branch
//! deletes are best-effort cleanup of refs that no longer affect slot state.
//! Re-running release is safe — each step is idempotent.
use anyhow::{Context, Result};
use std::path::Path;

use crate::cli::ReleaseArgs;
use crate::config::PoolConfig;
use crate::types::BranchName;
use crate::{fs_paths, git, mutex, slot, submodules};

pub fn run(pool_root: &Path, cfg: &PoolConfig, args: ReleaseArgs) -> Result<()> {
    let _pool_mu = mutex::FileLock::acquire(fs_paths::pool_mutex(pool_root))
        .context("acquiring pool mutex for release")?;

    // Lookup order: branch name (normal), then canonical slot id (operator
    // addressing a held slot by its on-disk id).
    let branch = BranchName::from(args.name.as_str());
    if let Some(entry) = slot::find_by_name(pool_root, cfg, &branch)? {
        return release_tail(&entry.path, &args.name);
    }
    let path = pool_root.join(&args.name);
    if path.is_dir() && slot::is_held_at(&path) {
        let name = git::current_branch(&path).unwrap_or_else(|| args.name.clone());
        return release_tail(&path, &name);
    }
    eprintln!("release '{}': no held slot (already released)", args.name);
    Ok(())
}

/// The release body without the pool mutex or the find-by-name lookup.
/// Idempotent — each step is a no-op if already done.
fn release_tail(slot_path: &Path, name: &str) -> Result<()> {
    // Detach HEAD — this flips held → idle. Can't delete the branch we're on,
    // so detach first. Pool mutex is held, so no race with concurrent acquires.
    let (detach_ok, _, detach_err) = git::detach_head(slot_path)?;
    if !detach_ok {
        eprintln!(
            "warn: detach HEAD failed in {}; branch '{}' may persist as a dangling ref. {}",
            slot_path.display(),
            name,
            detach_err
        );
    }
    // Best-effort: the branch may already be gone and there may be no `origin`
    // (local-only model). The detach above is what makes the slot idle; ref
    // deletion is just tidy-up, so failures here don't matter.
    let _ = git::branch_delete(slot_path, name);
    let _ = git::push_delete(slot_path, "origin", name);

    // Mirror parent cleanup into every submodule (incl. nested) — acquire created
    // a `<name>` branch in each so commits have a push-ready label; release un-creates.
    submodules::delete_branch_recursive(slot_path, name);

    eprintln!("released '{}'", name);
    Ok(())
}

