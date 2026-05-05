//! Host-level health check. Runs without a `--pool` argument.
use anyhow::Result;
use std::path::Path;
use std::process::Command;

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
    let dir = home().join(".worktree-pool");
    if !dir.exists() {
        return CheckResult::Warn(format!(
            "{} not present — first init will create it",
            dir.display()
        ));
    }
    let mut count = 0u32;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            if entry.path().join(".meta/config.yaml").exists() {
                count += 1;
            }
        }
    }
    CheckResult::Ok(format!("{} ({} pool(s))", dir.display(), count))
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

fn home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new("/").to_path_buf())
}
