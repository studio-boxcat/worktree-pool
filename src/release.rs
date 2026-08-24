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
use crate::types::LeaseName;
use crate::{fs_paths, git, mutex, slot, submodules};

pub fn run(pool_root: &Path, cfg: &PoolConfig, args: ReleaseArgs) -> Result<()> {
    let _pool_mu = mutex::FileLock::acquire(fs_paths::pool_mutex(pool_root))
        .context("acquiring pool mutex for release")?;

    // Lookup order: lease (normal), then canonical slot id — `--lease ios-0` is deliberately
    // accepted so an operator can release a slot straight off `ls` without resolving its lease.
    let lease = LeaseName::from(args.lease.as_str());
    if let Some(entry) = slot::find_by_lease(pool_root, cfg, &lease)? {
        return release_tail(&entry.path, &args.lease);
    }
    let path = pool_root.join(&args.lease);
    if path.is_dir() && slot::is_held_at(&path) {
        let held = git::current_branch(&path).unwrap_or_else(|| args.lease.clone());
        return release_tail(&path, &held);
    }
    eprintln!("release '{}': no held slot (already released)", args.lease);
    Ok(())
}

/// The release body without the pool mutex or the lease lookup. `lease` is also the slot's
/// branch ref, which is what the git cleanup below deletes. Idempotent — each step is a
/// no-op if already done.
fn release_tail(slot_path: &Path, lease: &str) -> Result<()> {
    // Detach HEAD — this flips held → idle. Can't delete the branch we're on,
    // so detach first. Pool mutex is held, so no race with concurrent acquires.
    let (detach_ok, _, detach_err) = git::detach_head(slot_path)?;
    if !detach_ok {
        eprintln!(
            "warn: detach HEAD failed in {}; branch '{}' may persist as a dangling ref. {}",
            slot_path.display(),
            lease,
            detach_err
        );
    }
    // Best-effort: the branch may already be gone and there may be no `origin`
    // (local-only model). The detach above is what makes the slot idle; ref
    // deletion is just tidy-up, so failures here don't matter.
    let _ = git::branch_delete(slot_path, lease);
    let _ = git::push_delete(slot_path, "origin", lease);

    // Mirror parent cleanup into every submodule (incl. nested) — acquire created
    // a `<lease>` branch in each so commits have a push-ready label; release un-creates.
    submodules::delete_branch_recursive(slot_path, lease);

    eprintln!("released '{lease}'");
    Ok(())
}

