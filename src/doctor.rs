//! Host-level health check. Runs without a `--pool` argument.
use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;

use crate::release::is_stale_index_lock;
use crate::{config, fs_paths, git, slot};

pub fn run() -> Result<()> {
    let mut warnings = 0u32;
    let mut errors = 0u32;
    let mut check = |name: &str, result: CheckResult| match result {
        CheckResult::Ok(detail) => println!("  ✓ {name}: {detail}"),
        CheckResult::Warn(detail) => {
            println!("  ! {name}: {detail}");
            warnings += 1;
        }
        CheckResult::Err(detail) => {
            println!("  ✗ {name}: {detail}");
            errors += 1;
        }
    };

    println!("worktree-pool doctor");
    check("arch", check_arch());
    check("git", check_git());
    check("pools dir", check_pools_dir());
    check("binary quarantine", check_quarantine());
    check("stale index.lock", check_stale_index_locks());

    println!();
    if errors > 0 {
        anyhow::bail!("{errors} error(s), {warnings} warning(s)");
    }
    println!("ok ({warnings} warning(s))");
    Ok(())
}

enum CheckResult {
    Ok(String),
    Warn(String),
    Err(String),
}

fn check_arch() -> CheckResult {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    if os == "macos" && arch == "aarch64" {
        CheckResult::Ok(format!("{os}/{arch}"))
    } else {
        CheckResult::Warn(format!("{os}/{arch} — only macOS/aarch64 is tested"))
    }
}

fn check_git() -> CheckResult {
    match Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            CheckResult::Ok(v)
        }
        Ok(o) => CheckResult::Err(format!(
            "git --version exited {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => CheckResult::Err(format!("git not found: {e}")),
    }
}

fn check_pools_dir() -> CheckResult {
    let dir = fs_paths::worktree_root();
    if !dir.exists() {
        return CheckResult::Warn(format!(
            "{} not present — first init will create it",
            dir.display()
        ));
    }
    let mut count = 0u32;
    fs_paths::for_each_pool_dir(|_| count += 1);
    CheckResult::Ok(format!("{} ({} pool(s))", dir.display(), count))
}

/// Scan every initialized pool's slots for git's per-worktree `index.lock`
/// matching the crashed-git signature. Read-only: reports counts and paths;
/// actual cleanup happens at the next acquire/release via `release::reclaim_stale`.
/// See [[lifecycle.md#crash-recovery]].
fn check_stale_index_locks() -> CheckResult {
    let mut stale: Vec<PathBuf> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    fs_paths::for_each_pool_dir(|pool_path| {
        let cfg = match config::load(&pool_path) {
            Ok(c) => c,
            Err(e) => {
                skipped.push(format!("{}: {e:#}", pool_path.display()));
                return;
            }
        };
        let entries = match slot::enumerate(&pool_path, &cfg) {
            Ok(es) => es,
            Err(e) => {
                skipped.push(format!("{}: enumerate: {e:#}", pool_path.display()));
                return;
            }
        };
        for e in entries {
            let Ok(gitdir) = git::worktree_gitdir(&e.path) else { continue };
            if is_stale_index_lock(&gitdir) {
                stale.push(fs_paths::worktree_index_lock(&gitdir));
            }
        }
    });
    let mut detail = if stale.is_empty() {
        "none".to_string()
    } else {
        let mut d = format!(
            "{} stale lock(s) — clear by running acquire/release on the pool (or `wt go <name>`):",
            stale.len()
        );
        for p in &stale {
            d.push_str("\n      ");
            d.push_str(&p.display().to_string());
        }
        d
    };
    // A broken pool config must NOT mask stale-lock findings in healthy pools.
    if !skipped.is_empty() {
        detail.push_str(&format!(
            "\n      (skipped {} pool(s) on config/enumerate error; first: {})",
            skipped.len(),
            skipped[0]
        ));
    }
    let warn = !stale.is_empty() || !skipped.is_empty();
    if warn {
        CheckResult::Warn(detail)
    } else {
        CheckResult::Ok(detail)
    }
}

fn check_quarantine() -> CheckResult {
    // The running binary's `xattr -l <self>`. Quarantine xattr would be `com.apple.quarantine`.
    // If we got here, we're already running, so the OS already accepted us — but a freshly
    // pulled binary on a coworker's box might still be quarantined before first run.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return CheckResult::Warn(format!("can't resolve current_exe: {e}")),
    };
    match Command::new("xattr").arg("-l").arg(&exe).output() {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("com.apple.quarantine") {
                CheckResult::Warn(format!(
                    "{} has com.apple.quarantine; clear with: xattr -d com.apple.quarantine {}",
                    exe.display(),
                    exe.display()
                ))
            } else {
                CheckResult::Ok(format!("clean ({})", exe.display()))
            }
        }
        Ok(_) => CheckResult::Warn("xattr exited nonzero".into()),
        Err(e) => CheckResult::Warn(format!("xattr not available: {e}")),
    }
}

