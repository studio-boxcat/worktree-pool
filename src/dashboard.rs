//! `ls` and `inspect` rendering.
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

use crate::cli::{InspectArgs, LsArgs, PathArgs};
use crate::config::PoolConfig;
use crate::{fs_paths, git, lock::Lock, slot};

pub fn ls(pool_root: &Path, cfg: &PoolConfig, args: LsArgs) -> Result<()> {
    let entries = slot::enumerate(pool_root, cfg)?;
    let mut rows: Vec<Row> = Vec::new();

    for entry in &entries {
        rows.push(build_row(entry)?);
    }

    // Add unmaterialized canonical slots up to max_slots so the table reflects capacity.
    let present: std::collections::HashSet<String> =
        entries.iter().map(|e| e.id.clone()).collect();
    let groups_for_listing: Vec<Option<String>> = if cfg.groups.is_empty() {
        vec![None]
    } else {
        cfg.groups.iter().map(|g| Some(g.clone())).collect()
    };
    for g in &groups_for_listing {
        for n in 0..cfg.max_slots {
            let id = slot::canonical_id(g.as_deref(), n);
            if !present.contains(&id) {
                rows.push(Row::fresh(id, g.clone()));
            }
        }
    }

    if args.git_status {
        for r in &mut rows {
            if r.state == State::Held {
                augment_with_git(r);
            }
        }
    }

    rows.sort_by(|a, b| {
        state_order(&a.state)
            .cmp(&state_order(&b.state))
            .then_with(|| a.id.cmp(&b.id))
    });

    print_table(&rows, args.git_status);
    Ok(())
}

pub fn path(pool_root: &Path, cfg: &PoolConfig, args: PathArgs) -> Result<()> {
    let Some(entry) = slot::find_by_name(pool_root, cfg, &args.name)? else {
        // Empty stderr + exit 1 so callers can `if wp path X >/dev/null; then`.
        std::process::exit(1);
    };
    println!("{}", entry.path.display());
    Ok(())
}

pub fn inspect(pool_root: &Path, cfg: &PoolConfig, args: InspectArgs) -> Result<()> {
    let entry = slot::find_by_name(pool_root, cfg, &args.name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no held slot with branch '{}' in {}",
            args.name,
            pool_root.display()
        )
    })?;

    let gitdir = git::worktree_gitdir(&entry.path)?;
    let lock_path = fs_paths::slot_lock(&gitdir);
    let lock = if lock_path.exists() {
        Some(Lock::read(&lock_path)?)
    } else {
        None
    };

    println!("# slot: {} (branch: {})", entry.id, args.name);
    println!("path: {}", entry.path.display());
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

    let (_, status, _) = git::run_lenient(&entry.path, &["status", "-sb"])?;
    println!("## git status -sb\n{}", status);
    println!();

    let range = format!("{}..HEAD", cfg.default_commit);
    let (ok, log, _) = git::run_lenient(&entry.path, &["log", "--oneline", "-20", &range])?;
    println!("## git log --oneline -20 {range}");
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

    let group = entry.group.clone().unwrap_or_else(|| "-".into());

    let name = if state == State::Held {
        git::current_branch(&entry.path).unwrap_or_else(|| "(detached)".into())
    } else {
        "-".into()
    };

    let full_sha = lock
        .as_ref()
        .map(|l| l.full_sha[..8.min(l.full_sha.len())].to_string())
        .unwrap_or_else(|| "-".into());

    Ok(Row {
        id: entry.id.clone(),
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

fn augment_with_git(row: &mut Row) {
    if row.path.as_os_str().is_empty() {
        return;
    }
    if let Ok((ok, porcelain, _)) = git::run_lenient(&row.path, &["status", "--porcelain"]) {
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
    }
    if let Ok((ok, ahead, _)) = git::run_lenient(
        &row.path,
        &["rev-list", "--count", "HEAD", "^refs/heads/main"],
    ) {
        if ok {
            row.ahead = ahead.trim().to_string();
        }
    }
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
