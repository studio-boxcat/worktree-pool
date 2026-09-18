//! Admin verbs: `unstick` (report mutex flock state) and `validate-gitmodules`.
//! Both report JSON — see [[../docs/cli.md#output-contract]].
//!
//! With OS-managed flocks (`std::fs::File::try_lock`), leftover mutex files
//! carry no semantic load — the kernel auto-releases the lock on process
//! death. `unstick` is therefore a read-only diagnostic: report which init
//! mutexes are currently held by a live process. There's no "force-clear" —
//! flock can't be released from outside the holding process (kill the holder
//! if you really need it gone).
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;

use crate::cli::UnstickArgs;
use crate::config::PoolConfig;
use crate::mutex;
use crate::output::{self, Outcome};

pub fn unstick(pool_root: &Path, args: UnstickArgs) -> Result<Outcome> {
    let pool_mutex_path = crate::fs_paths::pool_mutex(pool_root);
    // Absent file → null, distinct from a present-but-free mutex.
    let pool_mutex = if pool_mutex_path.exists() {
        json!({
            "path": output::path(&pool_mutex_path),
            "held": mutex::is_held(&pool_mutex_path),
        })
    } else {
        Value::Null
    };

    let init_dir = pool_root.join(".meta/init");
    let mut init_mutexes: Vec<Value> = Vec::new();
    if init_dir.exists() {
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

            init_mutexes.push(json!({
                "slot": slot_id,
                "path": output::path(&path),
                "held": mutex::is_held(&path),
            }));
        }
    }
    init_mutexes.sort_by(|a, b| a["slot"].as_str().cmp(&b["slot"].as_str()));

    // No held/free/total summary: derivable from the array, and two sources for
    // one fact drift.
    Ok(Outcome::json(json!({
        "pool_mutex": pool_mutex,
        "init_dir": output::path(&init_dir),
        "init_mutexes": init_mutexes,
    })))
}

/// Parse the source repo's `.gitmodules` and flag unknown `worktreePool*` keys
/// (typo guard — git silently accepts misspelled keys).
pub fn validate_gitmodules(cfg: &PoolConfig) -> Result<Outcome> {
    let path = cfg.source.join(".gitmodules");
    // Report shape is identical whether or not the file exists; `gitmodules: null`
    // is the "nothing to validate" case, so consumers need no second branch.
    if !path.exists() {
        return Ok(Outcome::json(
            json!({ "gitmodules": Value::Null, "tag_entries": 0, "unknown_keys": [] }),
        ));
    }

    let out = crate::git::config_file_list(&cfg.source, &path)?;

    let mut unknown_keys: Vec<String> = Vec::new();
    let mut tag_entries = 0u32;
    for (name, key, _value) in crate::submodules::iter_keys(&out) {
        if key == "worktreepooltag" {
            tag_entries += 1;
        } else if key.starts_with("worktreepool") {
            unknown_keys.push(format!("submodule.{name}.{key}"));
        }
    }

    let report = json!({
        "gitmodules": output::path(&path),
        "tag_entries": tag_entries,
        "unknown_keys": unknown_keys,
    });
    if unknown_keys.is_empty() {
        return Ok(Outcome::json(report));
    }
    // Report still goes to stdout — the caller wants the key list, not just the code.
    Ok(Outcome::json_failing(
        report,
        anyhow::anyhow!(
            "{} unknown worktreePool* key(s) in {}: {} — did you mean 'worktreePoolTag'?",
            unknown_keys.len(),
            path.display(),
            unknown_keys.join(", ")
        ),
    ))
}
