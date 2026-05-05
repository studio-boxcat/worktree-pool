//! `ls` and `inspect` rendering.
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

use crate::cli::{InspectArgs, LsArgs};
use crate::config::PoolConfig;
use crate::{fs_paths, git, lock::Lock, slot};

pub fn ls(pool_root: &Path, cfg: &PoolConfig, args: LsArgs) -> Result<()> {
    let entries = slot::enumerate(pool_root, cfg)?;
    let mut rows: Vec<Row> = Vec::new();

    // Held slots (anything with a renamed dir or a canonical dir whose lock exists).
    for entry in &entries {
        let row = build_row(entry)?;
        rows.push(row);
    }

    // Add unmaterialized canonical slots up to max_slots so the table reflects capacity.
    let canonical_present: std::collections::HashSet<String> = entries
        .iter()
        .filter_map(|e| match &e.kind {
            slot::SlotEntryKind::Canonical { group, n } => {
                Some(slot::canonical_id(group.as_deref(), *n))
            }
            slot::SlotEntryKind::Renamed => None,
        })
        .collect();
    let groups_for_listing: Vec<Option<String>> = if cfg.groups.is_empty() {
        vec![None]
    } else {
        cfg.groups.iter().map(|g| Some(g.clone())).collect()
    };
    for g in &groups_for_listing {
        for n in 0..cfg.max_slots {
            let id = slot::canonical_id(g.as_deref(), n);
            if !canonical_present.contains(&id) {
                rows.push(Row::fresh(id, g.clone()));
            }
        }
    }

    // Optionally augment held rows with git-status columns. Skip for idle/fresh.
    if args.git_status {
        for r in &mut rows {
            if r.state == State::Held {
                let _ = augment_with_git(r);
            }
        }
    }

    // Sort: held first (by name), then idle (by canonical id), then fresh.
    rows.sort_by(|a, b| {
        let order_a = state_order(&a.state);
        let order_b = state_order(&b.state);
        order_a.cmp(&order_b).then_with(|| a.id.cmp(&b.id))
    });

    print_table(&rows, args.git_status);
    Ok(())
}

pub fn inspect(pool_root: &Path, _cfg: &PoolConfig, args: InspectArgs) -> Result<()> {
    let slot_path = pool_root.join(&args.name);
    if !slot_path.exists() {
        anyhow::bail!("no slot named '{}' in {}", args.name, pool_root.display());
    }
    let gitdir = git::worktree_gitdir(&slot_path)?;
    let lock_path = fs_paths::slot_lock(&gitdir);
    let lock = if lock_path.exists() {
        Some(Lock::read(&lock_path)?)
    } else {
        None
    };

    println!("# slot: {}", args.name);
    println!("path: {}", slot_path.display());
    println!("gitdir: {}", gitdir.display());
    println!("lock: {}", lock_path.display());
    println!();

    println!("## lock");
    match &lock {
        Some(l) => {
            println!("started_at: {}", l.started_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            println!("full_sha: {}", l.full_sha);
            if let Some(g) = &l.group {
                println!("group: {g}");
            }
        }
        None => println!("(no lock — slot is idle)"),
    }
    println!();

    let (_, status, _) = git::run_lenient(&slot_path, &["status", "-sb"])?;
    println!("## git status -sb\n{}", status);
    println!();

    let (ok, log, _) = git::run_lenient(
        &slot_path,
        &["log", "--oneline", "-20", "origin/main..HEAD"],
    )?;
    println!("## git log --oneline -20 origin/main..HEAD");
    println!("{}", if ok && !log.is_empty() { log } else { "(none)".to_string() });
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Held,
    Idle,
    Fresh,
}

#[derive(Debug, Clone)]
struct Row {
    id: String,
    state: State,
    name: String,
    group: String,
    age: String,
    full_sha: String,
    dirty: String,
    untracked: String,
    ahead: String,
    path: PathBuf,
}

impl Row {
    fn fresh(id: String, group: Option<String>) -> Self {
        Self {
            id,
            state: State::Fresh,
            name: "-".into(),
            group: group.unwrap_or_else(|| "-".into()),
            age: "-".into(),
            full_sha: "-".into(),
            dirty: "-".into(),
            untracked: "-".into(),
            ahead: "-".into(),
            path: PathBuf::new(),
        }
    }
}

fn build_row(entry: &slot::SlotEntry) -> Result<Row> {
    // Look up the gitdir + lock to decide held vs idle.
    let gitdir_result = git::worktree_gitdir(&entry.path);
    let (state, lock, age) = match gitdir_result {
        Ok(gd) => {
            let lock_path = fs_paths::slot_lock(&gd);
            if lock_path.exists() {
                match Lock::read(&lock_path) {
                    Ok(l) => {
                        let age = format_age(l.started_at);
                        (State::Held, Some(l), age)
                    }
                    Err(_) => (State::Held, None, "?".into()),
                }
            } else {
                (State::Idle, None, "-".into())
            }
        }
        Err(_) => (State::Idle, None, "-".into()),
    };

    let group = lock
        .as_ref()
        .and_then(|l| l.group.clone())
        .or_else(|| match &entry.kind {
            slot::SlotEntryKind::Canonical { group: Some(g), .. } => Some(g.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "-".into());

    let name = match (&state, &entry.kind) {
        (State::Held, _) => entry.name.clone(),
        (_, slot::SlotEntryKind::Canonical { .. }) => "-".into(),
        _ => entry.name.clone(),
    };

    let full_sha = lock
        .as_ref()
        .map(|l| l.full_sha[..8.min(l.full_sha.len())].to_string())
        .unwrap_or_else(|| "-".into());

    Ok(Row {
        id: entry.name.clone(),
        state,
        name,
        group,
        age,
        full_sha,
        dirty: "-".into(),
        untracked: "-".into(),
        ahead: "-".into(),
        path: entry.path.clone(),
    })
}

fn augment_with_git(row: &mut Row) -> Result<()> {
    if row.path.as_os_str().is_empty() {
        return Err(anyhow!("no path"));
    }
    let (ok, porcelain, _) = git::run_lenient(&row.path, &["status", "--porcelain"])?;
    if ok {
        let mut dirty = 0u32;
        let mut untracked = 0u32;
        for line in porcelain.lines().filter(|l| !l.is_empty()) {
            if line.starts_with("??") {
                untracked += 1;
            } else {
                dirty += 1;
            }
        }
        row.dirty = dirty.to_string();
        row.untracked = untracked.to_string();
    }
    let (ok, ahead, _) = git::run_lenient(
        &row.path,
        &["rev-list", "--count", "HEAD", "^refs/remotes/origin/main"],
    )?;
    if ok {
        row.ahead = ahead.trim().to_string();
    }
    Ok(())
}

fn format_age(then: DateTime<Utc>) -> String {
    let secs = (Utc::now() - then).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn state_order(s: &State) -> u8 {
    match s {
        State::Held => 0,
        State::Idle => 1,
        State::Fresh => 2,
    }
}

fn print_table(rows: &[Row], with_git: bool) {
    let mut headers = vec!["ID", "STATE", "NAME", "GROUP", "AGE", "SHA"];
    if with_git {
        headers.extend(["DIRTY", "UNTRK", "AHEAD"]);
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let mut row = vec![
                r.id.clone(),
                state_label(&r.state).into(),
                r.name.clone(),
                r.group.clone(),
                r.age.clone(),
                r.full_sha.clone(),
            ];
            if with_git {
                row.extend([r.dirty.clone(), r.untracked.clone(), r.ahead.clone()]);
            }
            row
        })
        .collect();

    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            std::cmp::max(
                h.len(),
                cells.iter().map(|r| r[i].len()).max().unwrap_or(0),
            )
        })
        .collect();

    let row_str = |cells: &[String]| -> String {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}", c = c, w = w))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!(
        "{}",
        row_str(&headers.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    );
    println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  "));
    for c in &cells {
        println!("{}", row_str(c));
    }
}

fn state_label(s: &State) -> &'static str {
    match s {
        State::Held => "held",
        State::Idle => "idle",
        State::Fresh => "fresh",
    }
}

#[allow(dead_code)]
fn _unused(_: &Path) -> Result<()> {
    Ok(())
}

// Hide the leftover `Context` import pull-in.
#[allow(dead_code)]
fn _force_context_import(p: &Path) -> Result<()> {
    std::fs::read_to_string(p).context("read")?;
    Ok(())
}
