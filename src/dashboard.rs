//! Read-only subcommands: `ls` (slot array, with optional parallel git status),
//! `inspect` (one slot's git state), `path` (slot-id lookup).
//! `ls`/`inspect` report JSON; `path` reports a bare line. See [[../docs/cli.md#output-contract]].
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::cli::{InspectArgs, LsArgs, PathArgs};
use crate::config::PoolConfig;
use crate::output::{self, Outcome};
use crate::types::{GroupName, LeaseName};
use crate::{git, parallel, slot};

pub fn ls(pool_root: &Path, cfg: &PoolConfig, args: LsArgs) -> Result<Outcome> {
    let entries = slot::enumerate(pool_root, cfg)?;
    let mut rows: Vec<Row> = entries.iter().map(build_row).collect();

    // Add unmaterialized canonical slots up to max_slots so the listing reflects capacity.
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
        // Held rows always carry a path; `unwrap_or_default` keeps this list
        // index-aligned with the held rows below even if that ever stops holding
        // (an empty path yields empty counts rather than shifting every row).
        let held_paths: Vec<PathBuf> = rows
            .iter()
            .filter(|r| r.state == State::Held)
            .map(|r| r.path.clone().unwrap_or_default())
            .collect();
        let mut aug_iter = parallel::map(&held_paths, |p| compute_git_counts(p)).into_iter();
        for r in &mut rows {
            if r.state == State::Held
                && let Some(g) = aug_iter.next()
            {
                r.git = Some(g);
            }
        }
    }

    rows.sort_by(|a, b| {
        state_order(&a.state)
            .cmp(&state_order(&b.state))
            .then_with(|| a.id.cmp(&b.id))
    });

    Ok(Outcome::json(rows.iter().map(Row::to_json).collect()))
}

pub fn path(pool_root: &Path, cfg: &PoolConfig, args: PathArgs) -> Result<Outcome> {
    let name = LeaseName::from(args.lease.as_str());
    let Some(entry) = slot::find_by_lease(pool_root, cfg, &name)? else {
        // Empty stderr + exit 1 so callers can `if wp path X >/dev/null; then`.
        return Ok(Outcome::silent_exit(1));
    };
    Ok(Outcome::line(entry.path.display().to_string()))
}

pub fn inspect(pool_root: &Path, cfg: &PoolConfig, args: InspectArgs) -> Result<Outcome> {
    let name = LeaseName::from(args.lease.as_str());
    let entry = slot::find_by_lease(pool_root, cfg, &name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no held slot with branch '{}' in {}",
            args.lease,
            pool_root.display()
        )
    })?;

    let gitdir = git::worktree_gitdir(&entry.path)?;
    let sha = git::run(&entry.path, &["rev-parse", "HEAD"]).ok();
    let (_, status, _) = git::run_lenient(&entry.path, &["status", "-sb"])?;

    // `log` is the slot's work: commits on the lease branch not in the pool's base.
    let range = format!("{}..HEAD", cfg.default_commit);
    let (ok, log, _) = git::run_lenient(&entry.path, &["log", "--oneline", "-20", &range])?;

    Ok(Outcome::json(json!({
        "id": entry.id.as_str(),
        "lease": args.lease,
        "group": output::opt_str(entry.group.as_ref().map(GroupName::as_str)),
        "path": output::path(&entry.path),
        "gitdir": output::path(&gitdir),
        "sha": output::opt_str(sha.as_deref()),
        "status": output::lines(&status),
        "base": cfg.default_commit.as_str(),
        "log": if ok { output::lines(&log) } else { Value::Null },
    })))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Held,
    Idle,
    Fresh,
}

/// `git status --porcelain` + `rev-list` counts for a held slot. `None` per field
/// when its git call failed.
#[derive(Debug, Clone, Default)]
struct GitCounts {
    dirty: Option<u32>,
    untracked: Option<u32>,
    ahead: Option<u32>,
}

#[derive(Debug, Clone)]
struct Row {
    id: String,
    state: State,
    lease: Option<String>,
    group: Option<String>,
    sha: Option<String>,
    /// `None` unless `--git-status` was passed.
    git: Option<GitCounts>,
    /// `None` for fresh slots — nothing is materialized on disk yet.
    path: Option<PathBuf>,
}

impl Row {
    fn fresh(id: crate::types::SlotId, group: Option<GroupName>) -> Self {
        Self {
            id: id.to_string(),
            state: State::Fresh,
            lease: None,
            group: group.map(|g| g.to_string()),
            sha: None,
            git: None,
            path: None,
        }
    }

    fn to_json(&self) -> Value {
        let mut o = json!({
            "id": self.id,
            "state": state_label(&self.state),
            "lease": output::opt_str(self.lease.as_deref()),
            "group": output::opt_str(self.group.as_deref()),
            "sha": output::opt_str(self.sha.as_deref()),
            "path": self.path.as_deref().map_or(Value::Null, output::path),
        });
        if let Some(g) = &self.git {
            o["git"] = json!({
                "dirty": g.dirty,
                "untracked": g.untracked,
                "ahead": g.ahead,
            });
        }
        o
    }
}

fn build_row(entry: &slot::SlotEntry) -> Row {
    let branch = git::current_branch(&entry.path);
    let state = if branch.is_some() { State::Held } else { State::Idle };

    // Short sha only for held slots — an idle slot's detached HEAD is pool
    // bookkeeping, not something a caller acts on.
    let sha = (state == State::Held)
        .then(|| git::run(&entry.path, &["rev-parse", "HEAD"]).ok())
        .flatten()
        .map(|sha| sha[..8.min(sha.len())].to_string());

    Row {
        id: entry.id.to_string(),
        state,
        lease: branch,
        group: entry.group.as_ref().map(GroupName::to_string),
        sha,
        git: None,
        path: Some(entry.path.clone()),
    }
}

fn compute_git_counts(path: &Path) -> GitCounts {
    let mut counts = GitCounts::default();
    if path.as_os_str().is_empty() {
        return counts;
    }
    if let Ok((true, porcelain, _)) = git::run_lenient(path, &["status", "--porcelain"]) {
        let (mut d, mut u) = (0u32, 0u32);
        for line in porcelain.lines().filter(|l| !l.is_empty()) {
            if line.starts_with("??") {
                u += 1;
            } else {
                d += 1;
            }
        }
        counts.dirty = Some(d);
        counts.untracked = Some(u);
    }
    if let Ok((true, a, _)) =
        git::run_lenient(path, &["rev-list", "--count", "HEAD", "^refs/heads/main"])
    {
        counts.ahead = a.trim().parse().ok();
    }
    counts
}

fn state_order(s: &State) -> u8 {
    match s {
        State::Held => 0,
        State::Idle => 1,
        State::Fresh => 2,
    }
}

fn state_label(s: &State) -> &'static str {
    match s {
        State::Held => "held",
        State::Idle => "idle",
        State::Fresh => "fresh",
    }
}
