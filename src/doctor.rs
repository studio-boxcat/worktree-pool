//! Host-level health check. Runs without a `--pool` argument.
use anyhow::Result;
use std::process::Command;

use crate::{config, fs_paths, git};

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
    check("pools", check_pools());

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

/// Walk every initialized pool and validate: config parses (schema check is
/// inside `config::load`); `source` path exists and is a readable git repo.
/// Counts unhealthy pools; reports the first failure inline.
fn check_pools() -> CheckResult {
    let mut total = 0u32;
    let mut bad: Vec<String> = Vec::new();
    fs_paths::for_each_pool_dir(|pool_path| {
        total += 1;
        let key = pool_path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        let cfg = match config::load(&pool_path) {
            Ok(c) => c,
            Err(e) => {
                bad.push(format!("{key}: config: {e:#}"));
                return;
            }
        };
        if !cfg.source.exists() {
            bad.push(format!("{key}: source {} missing", cfg.source.display()));
            return;
        }
        if git::source_gitdir(&cfg.source).is_err() {
            bad.push(format!("{key}: source {} is not a git repo", cfg.source.display()));
        }
    });
    if total == 0 {
        return CheckResult::Ok("no pools initialized yet".into());
    }
    if bad.is_empty() {
        return CheckResult::Ok(format!("{total} pool(s), all healthy"));
    }
    let mut detail = format!("{}/{total} unhealthy:", bad.len());
    for line in &bad {
        detail.push_str("\n      ");
        detail.push_str(line);
    }
    CheckResult::Warn(detail)
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

