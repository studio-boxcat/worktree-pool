//! Read-only subcommands: `ls` (slot table, with optional parallel git status),
//! `inspect` (one slot's git state), `path` (slot-id lookup).
use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::cli::{InspectArgs, LsArgs, PathArgs};
use crate::config::PoolConfig;
use crate::types::{BranchName, GroupName};
use crate::{git, parallel, slot};

pub fn ls(pool_root: &Path, cfg: &PoolConfig, args: LsArgs) -> Result<()> {
    let entries = slot::enumerate(pool_root, cfg)?;
    let mut rows: Vec<Row> = Vec::new();

    for entry in &entries {
        rows.push(build_row(entry));
    }

    // Add unmaterialized canonical slots up to max_slots so the table reflects capacity.
    let present: std::collections::HashSet<String> =
        entries.iter().map(|e| e.id.to_string()).collect();
    let groups_for_listing: Vec<Option<GroupName>> = if cfg.groups.is_empty() {
        vec![None]
    } else {
        cfg.groups.iter().map(|g| Some(g.clone())).collect()
    };
    for g in &groups_for_listing {
        for n in 0..cfg.max_slots {
            let id = slot::canonical_id(g.as_ref(), n);
            if !present.contains(id.as_str()) {
                rows.push(Row::fresh(id, g.clone()));
            }
        }
    }

    if args.git_status {
        // Parallel: H held slots × 2 git spawns each is the wall-clock
        // bottleneck. Compute deltas immutably + parallel, apply sequentially.
        let held_paths: Vec<PathBuf> = rows
            .iter()
            .filter(|r| r.state == State::Held)
            .map(|r| r.path.clone())
            .collect();
        let mut aug_iter = parallel::map(&held_paths, |p| compute_git_columns(p)).into_iter();
        for r in &mut rows {
            if r.state == State::Held
                && let Some((dirty, untracked, ahead)) = aug_iter.next()
            {
                r.dirty = dirty;
                r.untracked = untracked;
                r.ahead = ahead;
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
    let name = BranchName::from(args.name.as_str());
    let Some(entry) = slot::find_by_name(pool_root, cfg, &name)? else {
        // Empty stderr + exit 1 so callers can `if wp path X >/dev/null; then`.
        std::process::exit(1);
    };
    println!("{}", entry.path.display());
    Ok(())
}

pub fn inspect(pool_root: &Path, cfg: &PoolConfig, args: InspectArgs) -> Result<()> {
    let name = BranchName::from(args.name.as_str());
    let entry = slot::find_by_name(pool_root, cfg, &name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no held slot with branch '{}' in {}",
            args.name,
            pool_root.display()
        )
    })?;

    let gitdir = git::worktree_gitdir(&entry.path)?;
    let sha = git::run(&entry.path, &["rev-parse", "HEAD"]).unwrap_or_else(|_| "?".into());

    println!("# slot: {} (branch: {})", entry.id, args.name);
    println!("path: {}", entry.path.display());
    println!("gitdir: {}", gitdir.display());
    println!("sha: {}", sha);
    if let Some(g) = &entry.group {
        println!("group: {g}");
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
    full_sha: String,
    dirty: String,
    untracked: String,
    ahead: String,
    path: PathBuf,
}

impl Row {
    fn fresh(id: crate::types::SlotId, group: Option<GroupName>) -> Self {
        Self {
            id: id.to_string(),
            state: State::Fresh,
            name: "-".into(),
            group: group.map(|g| g.to_string()).unwrap_or_else(|| "-".into()),
            full_sha: "-".into(),
            dirty: "-".into(),
            untracked: "-".into(),
            ahead: "-".into(),
            path: PathBuf::new(),
        }
    }
}

fn build_row(entry: &slot::SlotEntry) -> Row {
    let branch = git::current_branch(&entry.path);
    let state = if branch.is_some() { State::Held } else { State::Idle };
    let group = entry.group.as_ref().map(GroupName::as_str).unwrap_or("-").to_string();
    let name = branch.unwrap_or_else(|| "-".into());

    let full_sha = if state == State::Held {
        git::run(&entry.path, &["rev-parse", "HEAD"])
            .map(|sha| sha[..8.min(sha.len())].to_string())
            .unwrap_or_else(|_| "?".into())
    } else {
        "-".into()
    };

    Row {
        id: entry.id.to_string(),
        state,
        name,
        group,
        full_sha,
        dirty: "-".into(),
        untracked: "-".into(),
        ahead: "-".into(),
        path: entry.path.clone(),
    }
}

/// Returns `(dirty, untracked, ahead)` columns for the held slot at `path`.
/// Each defaults to `"-"` when its git call failed or the path is empty.
fn compute_git_columns(path: &Path) -> (String, String, String) {
    let mut dirty = "-".to_string();
    let mut untracked = "-".to_string();
    let mut ahead = "-".to_string();
    if path.as_os_str().is_empty() {
        return (dirty, untracked, ahead);
    }
    if let Ok((true, porcelain, _)) = git::run_lenient(path, &["status", "--porcelain"]) {
        let mut d = 0u32;
        let mut u = 0u32;
        for line in porcelain.lines().filter(|l| !l.is_empty()) {
            if line.starts_with("??") {
                u += 1;
            } else {
                d += 1;
            }
        }
        dirty = d.to_string();
        untracked = u.to_string();
    }
    if let Ok((true, a, _)) = git::run_lenient(
        path,
        &["rev-list", "--count", "HEAD", "^refs/heads/main"],
    ) {
        ahead = a.trim().to_string();
    }
    (dirty, untracked, ahead)
}

fn state_order(s: &State) -> u8 {
    match s {
        State::Held => 0,
        State::Idle => 1,
        State::Fresh => 2,
    }
}

fn print_table(rows: &[Row], with_git: bool) {
    let mut headers = vec!["ID", "STATE", "NAME", "GROUP", "SHA"];
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
            .map(|(c, w)| format!("{c:<w$}"))
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
