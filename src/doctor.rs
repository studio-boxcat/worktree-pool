//! Host-level health check. Runs without a `--pool` argument. Reports boxcat-ts-core's
//! `SectionResult[]` (doctor.ts) — the wire format boxcat-devenv's doctor merges into
//! its own report. See [[../docs/cli.md#output-contract]].
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

use crate::output::Outcome;
use crate::{config, fs_paths, git};

pub fn run() -> Result<Outcome> {
    let pools = match fs_paths::try_worktree_root() {
        Some(root) => vec![check_pools_dir(&root), check_pools()],
        None => vec![fail(
            "pools dir",
            "WORKTREE_ROOT unset → boxcat/env.zsh (config) defines it; run from a shell that sourced .zshenv",
        )],
    };
    let sections = [
        Section {
            name: "Host",
            checks: vec![check_arch(), check_git(), check_quarantine()],
        },
        Section {
            name: "Pools",
            checks: pools,
        },
    ];
    let checks = || sections.iter().flat_map(|s| &s.checks);

    let report = sections.iter().map(Section::to_json).collect::<Value>();

    let errors = checks().filter(|c| c.status == Status::Error).count();
    let warnings = checks().filter(|c| c.status == Status::Warn).count();
    if errors == 0 {
        return Ok(Outcome::json(report));
    }
    // Report still goes to stdout — the merging consumer needs the failing checks.
    Ok(Outcome::json_failing(
        report,
        anyhow::anyhow!("{errors} error(s), {warnings} warning(s)"),
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Error,
}

impl Status {
    /// `CheckStatus` in check-result.ts.
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    message: String,
}

struct Section {
    name: &'static str,
    checks: Vec<Check>,
}

impl Section {
    fn to_json(&self) -> Value {
        let results: Vec<Value> = self
            .checks
            .iter()
            .map(|c| json!({ "name": c.name, "status": c.status.as_str(), "message": c.message }))
            .collect();
        json!({ "name": self.name, "results": results })
    }
}

fn ok(name: &'static str, message: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Ok,
        message: message.into(),
    }
}

fn warn(name: &'static str, message: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Warn,
        message: message.into(),
    }
}

/// A fix goes in the message as `message → fix`, the way check-result.ts renders one.
fn fail(name: &'static str, message: impl Into<String>) -> Check {
    Check {
        name,
        status: Status::Error,
        message: message.into(),
    }
}

fn check_arch() -> Check {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    if os == "macos" && arch == "aarch64" {
        ok("arch", format!("{os}/{arch}"))
    } else {
        warn(
            "arch",
            format!("{os}/{arch} — only macOS/aarch64 is tested"),
        )
    }
}

fn check_git() -> Check {
    match Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => ok("git", String::from_utf8_lossy(&o.stdout).trim()),
        Ok(o) => fail(
            "git",
            format!(
                "git --version exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        ),
        Err(e) => fail("git", format!("git not found: {e}")),
    }
}

fn check_pools_dir(dir: &Path) -> Check {
    if !dir.exists() {
        return warn(
            "pools dir",
            format!("{} not present — first init will create it", dir.display()),
        );
    }
    let mut count = 0u32;
    fs_paths::for_each_pool_dir(|_| count += 1);
    ok(
        "pools dir",
        format!("{} ({} pool(s))", dir.display(), count),
    )
}

/// Walk every initialized pool and validate: config parses (schema check is
/// inside `config::load`); `source` path exists and is a readable git repo.
/// Counts unhealthy pools; reports the first failure inline.
fn check_pools() -> Check {
    let mut total = 0u32;
    let mut bad: Vec<String> = Vec::new();
    fs_paths::for_each_pool_dir(|pool_path| {
        total += 1;
        let key = pool_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?");
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
            bad.push(format!(
                "{key}: source {} is not a git repo",
                cfg.source.display()
            ));
        }
    });
    if total == 0 {
        return ok("pools", "no pools initialized yet");
    }
    if bad.is_empty() {
        return ok("pools", format!("{total} pool(s), all healthy"));
    }
    let mut detail = format!("{}/{total} unhealthy:", bad.len());
    for line in &bad {
        detail.push_str("\n      ");
        detail.push_str(line);
    }
    warn("pools", detail)
}

fn check_quarantine() -> Check {
    // The running binary's `xattr -l <self>`. Quarantine xattr would be `com.apple.quarantine`.
    // If we got here, we're already running, so the OS already accepted us — but a freshly
    // pulled binary on a coworker's box might still be quarantined before first run.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return warn(
                "binary quarantine",
                format!("can't resolve current_exe: {e}"),
            );
        }
    };
    match Command::new("xattr").arg("-l").arg(&exe).output() {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("com.apple.quarantine") {
                warn(
                    "binary quarantine",
                    format!(
                        "{} has com.apple.quarantine → xattr -d com.apple.quarantine {}",
                        exe.display(),
                        exe.display()
                    ),
                )
            } else {
                ok("binary quarantine", format!("clean ({})", exe.display()))
            }
        }
        Ok(_) => warn("binary quarantine", "xattr exited nonzero"),
        Err(e) => warn("binary quarantine", format!("xattr not available: {e}")),
    }
}
