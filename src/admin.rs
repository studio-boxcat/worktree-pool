//! Admin verbs: `unstick` (clear stale init mutexes) and `validate-gitmodules`.
use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::cli::UnstickArgs;
use crate::config::PoolConfig;
use crate::mutex;

pub fn unstick(pool_root: &Path, _cfg: &PoolConfig, args: UnstickArgs) -> Result<()> {
    let init_dir = pool_root.join(".meta/init");
    if !init_dir.exists() {
        println!("no init mutexes present at {}", init_dir.display());
        return Ok(());
    }

    let now = SystemTime::now();
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
        let m = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("  skip {}: {e}", path.display());
                continue;
            }
        };
        let mtime = m.modified().context("mtime")?;
        let age = now.duration_since(mtime).unwrap_or(Duration::ZERO);

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

    // Use `git config --file <.gitmodules> --list` to leverage git's parser.
    let out = crate::git::run(&cfg.source, &["config", "--file", ".gitmodules", "--list"])?;

    let mut warnings = 0u32;
    let mut tag_count = 0u32;
    for line in out.lines() {
        // Lines look like `submodule.<name>.<key>=<value>`.
        let Some(rest) = line.strip_prefix("submodule.") else {
            continue;
        };
        let Some((sub_name_and_key, _value)) = rest.split_once('=') else {
            continue;
        };
        let Some((_sub_name, key)) = sub_name_and_key.rsplit_once('.') else {
            continue;
        };

        if key == "worktreepooltag" {
            tag_count += 1;
        } else if key.starts_with("worktreepool") {
            // Anything else with the worktreepool prefix is a typo.
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
