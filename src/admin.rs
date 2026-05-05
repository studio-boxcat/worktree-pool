//! Admin verbs: `unstick` (clear stale init mutexes) and `validate-gitmodules`.
use anyhow::{Context, Result};
use std::path::Path;

use crate::cli::UnstickArgs;
use crate::config::PoolConfig;
use crate::mutex;

pub fn unstick(pool_root: &Path, _cfg: &PoolConfig, args: UnstickArgs) -> Result<()> {
    // Pool-wide mutex: report status, force-clear if `--pool-mutex` was passed.
    let pool_mutex_path = crate::fs_paths::pool_mutex(pool_root);
    if pool_mutex_path.exists() {
        let age = mutex::age_of(&pool_mutex_path).map(|d| d.as_secs()).unwrap_or(0);
        if args.pool_mutex {
            std::fs::remove_file(&pool_mutex_path)
                .with_context(|| format!("rm {}", pool_mutex_path.display()))?;
            println!(
                "force-cleared pool mutex {} (age {}s)",
                pool_mutex_path.display(),
                age
            );
        } else {
            println!(
                "pool mutex held: {} (age {}s, stale-after {}s — auto-reclaim threshold; \
                 pass --pool-mutex to force-clear)",
                pool_mutex_path.display(),
                age,
                mutex::POOL_MUTEX_STALE_AFTER.as_secs()
            );
        }
    }

    let init_dir = pool_root.join(".meta/init");
    if !init_dir.exists() {
        println!("no init mutexes present at {}", init_dir.display());
        return Ok(());
    }

    let mut total = 0u32;
    let mut cleared = 0u32;

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
        let age = match mutex::age_of(&path) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("  skip {}: {e:#}", path.display());
                continue;
            }
        };

        if age >= mutex::STALE_AFTER {
            std::fs::remove_file(&path)
                .with_context(|| format!("rm {}", path.display()))?;
            cleared += 1;
            println!(
                "cleared stale mutex {} (age {}s)",
                slot_id,
                age.as_secs()
            );
        } else {
            println!(
                "live mutex {} (age {}s, threshold {}s)",
                slot_id,
                age.as_secs(),
                mutex::STALE_AFTER.as_secs()
            );
        }
    }

    println!(
        "unstick: {cleared} cleared, {total} total {}",
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
