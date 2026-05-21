//! Admin verbs: `unstick` (report mutex flock state) and `validate-gitmodules`.
//!
//! With OS-managed flocks (`std::fs::File::try_lock`), leftover mutex files
//! carry no semantic load — the kernel auto-releases the lock on process
//! death. `unstick` is therefore a read-only diagnostic: report which init
//! mutexes are currently held by a live process. There's no "force-clear" —
//! flock can't be released from outside the holding process (kill the holder
//! if you really need it gone).
use anyhow::{Context, Result};
use std::path::Path;

use crate::cli::UnstickArgs;
use crate::config::PoolConfig;
use crate::mutex;

pub fn unstick(pool_root: &Path, _cfg: &PoolConfig, args: UnstickArgs) -> Result<()> {
    let pool_mutex_path = crate::fs_paths::pool_mutex(pool_root);
    if pool_mutex_path.exists() {
        if mutex::is_held(&pool_mutex_path) {
            println!(
                "pool mutex HELD: {} (live holder; kill the process if it's stuck)",
                pool_mutex_path.display()
            );
        } else {
            println!(
                "pool mutex free: {} (no live holder)",
                pool_mutex_path.display()
            );
        }
    }

    let init_dir = pool_root.join(".meta/init");
    if !init_dir.exists() {
        println!("no init mutexes present at {}", init_dir.display());
        return Ok(());
    }

    let mut total = 0u32;
    let mut held = 0u32;

    for entry in std::fs::read_dir(&init_dir)
        .with_context(|| format!("read_dir {}", init_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let slot_id = name.strip_suffix(".lock").unwrap_or(&name).to_string();

        if let Some(target) = &args.slot
            && &slot_id != target
        {
            continue;
        }

        total += 1;
        if mutex::is_held(&path) {
            held += 1;
            println!("init mutex {}: HELD", slot_id);
        } else {
            println!("init mutex {}: free", slot_id);
        }
    }

    println!(
        "unstick: {held} held, {} free, {total} total {}",
        total - held,
        if args.slot.is_some() { "(filtered)" } else { "" }
    );
    Ok(())
}

/// Parse the source repo's `.gitmodules` and warn on unknown `worktreePool*` keys
/// (typo guard — git silently accepts misspelled keys).
pub fn validate_gitmodules(_pool_root: &Path, cfg: &PoolConfig) -> Result<()> {
    let path = cfg.source.join(".gitmodules");
    if !path.exists() {
        println!("no .gitmodules at {} — nothing to validate", path.display());
        return Ok(());
    }

    let out = crate::git::run(&cfg.source, &["config", "--file", ".gitmodules", "--list"])?;

    let mut warnings = 0u32;
    let mut tag_count = 0u32;
    for (_name, key, _value) in crate::submodules::iter_keys(&out) {
        if key == "worktreepooltag" {
            tag_count += 1;
        } else if key.starts_with("worktreepool") {
            eprintln!("warn: unknown key 'submodule.*.{key}'; did you mean 'worktreePoolTag'?");
            warnings += 1;
        }
    }

    println!("validate-gitmodules: {tag_count} `worktreePoolTag` entries, {warnings} warning(s)");
    if warnings > 0 {
        anyhow::bail!("{warnings} unknown worktreePool* key(s) in {}", path.display());
    }
    Ok(())
}
