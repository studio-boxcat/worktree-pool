//! Slot identity, enumeration, and idle-slot picking.
//!
//! A slot is **always** at its canonical path `<pool>/{group}-{N}` (or
//! `<pool>/slot-{N}` for groupless pools). It's **held** iff HEAD is on a
//! branch; **idle** when HEAD is detached. The user-given name lives in the
//! branch ref inside the slot — see [[../docs/lifecycle.md]].
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::PoolConfig;
use crate::git;

/// Canonical idle slot id: `{group}-{N}` when grouped, else `slot-{N}`.
pub fn canonical_id(group: Option<&str>, n: u32) -> String {
    match group {
        Some(g) => format!("{g}-{n}"),
        None => format!("slot-{n}"),
    }
}

/// Resolve the group to use for an acquire. If pool has groups, defaults to the first.
/// Validates that the requested group is one of the configured groups.
pub fn resolve_group<'a>(cfg: &'a PoolConfig, requested: Option<&str>) -> Result<Option<&'a str>> {
    if cfg.groups.is_empty() {
        if requested.is_some() {
            bail!("pool has no groups configured; --group is not allowed");
        }
        return Ok(None);
    }
    match requested {
        Some(g) => match cfg.groups.iter().find(|x| x.as_str() == g) {
            Some(found) => Ok(Some(found.as_str())),
            None => bail!(
                "unknown group '{g}'; pool's groups: {}",
                cfg.groups.join(", ")
            ),
        },
        None => {
            let first = cfg.groups[0].as_str();
            eprintln!("--group not given; defaulting to '{first}' (first of: {})", cfg.groups.join(", "));
            Ok(Some(first))
        }
    }
}

/// A canonical slot N that's currently acquirable — either fresh (no dir on
/// disk) or recycled-idle (dir exists, HEAD detached). Acquire iterates these
/// in order, tries the init mutex on each, and uses `is_fresh` to pick
/// between `worktree add` (fresh) and `reset --hard` (recycled).
#[derive(Debug, Clone, Copy)]
pub struct Acquirable {
    pub n: u32,
    pub is_fresh: bool,
}

/// Acquirable canonical slot Ns in lowest-N-first order.
///
/// **Fresh vs recycled in over-provisioned pools.** Fresh creation is bounded
/// by `0..max_slots` (never grow the pool past max_slots). Recycled-idle dirs
/// at N >= max_slots can exist (max_slots was reduced after materialization)
/// and are surfaced here as acquirable — reusing them eats down the surplus.
pub fn acquirable(
    pool_root: &Path,
    group: Option<&str>,
    max_slots: u32,
    entries: &[SlotEntry],
) -> Vec<Acquirable> {
    let mut out: Vec<Acquirable> = Vec::new();
    for n in 0..max_slots {
        let id = canonical_id(group, n);
        let p = pool_root.join(&id);
        if !p.exists() {
            out.push(Acquirable { n, is_fresh: true });
        } else if !is_held_at(&p) {
            out.push(Acquirable { n, is_fresh: false });
        }
    }
    for entry in entries {
        if entry.group.as_deref() == group
            && entry.n >= max_slots
            && !is_held_at(&entry.path)
        {
            out.push(Acquirable { n: entry.n, is_fresh: false });
        }
    }
    out.sort_unstable_by_key(|a| a.n);
    out
}

/// True iff the slot at `path` has HEAD on a branch (held).
/// Reads the gitdir's `HEAD` file directly (no subprocess). Returns `true`
/// on resolution or read failure — refuses to stomp on a dir whose state
/// we can't determine.
pub fn is_held_at(path: &Path) -> bool {
    let gitdir = match git::worktree_gitdir(path) {
        Ok(gd) => gd,
        Err(_) => return true,
    };
    match std::fs::read_to_string(gitdir.join("HEAD")) {
        Ok(content) => content.is_empty() || content.starts_with("ref:"),
        Err(_) => true,
    }
}

/// Count held slots in `group` — canonical slots whose HEAD is on a branch.
pub fn count_held_in_group(entries: &[SlotEntry], group: Option<&str>) -> usize {
    entries
        .iter()
        .filter(|e| e.group.as_deref() == group && is_held_at(&e.path))
        .count()
}

/// Look up a held slot by branch name. Scans canonical slots, matches by
/// `git symbolic-ref --short HEAD`. None if no held slot has branch `name`
/// (already released, never acquired, or HEAD detached).
pub fn find_by_name(
    pool_root: &Path,
    cfg: &PoolConfig,
    name: &str,
) -> Result<Option<SlotEntry>> {
    for entry in enumerate(pool_root, cfg)? {
        if !is_held_at(&entry.path) {
            continue;
        }
        if git::current_branch(&entry.path).as_deref() == Some(name) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// Enumerate canonical slot dirs under `pool_root`. Skips `.meta`, dotfiles,
/// non-directories, and anything that doesn't match the `{group}-{N}` /
/// `slot-{N}` pattern. Pool-internal: any non-canonical entry is operator
/// noise, not a held slot (held slots live at their canonical path).
pub fn enumerate(pool_root: &Path, cfg: &PoolConfig) -> Result<Vec<SlotEntry>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(pool_root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).context(format!("read_dir {}", pool_root.display())),
    };
    for entry in rd {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".meta" || name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some((group, n)) = classify(&name, cfg) else {
            continue;
        };
        out.push(SlotEntry {
            id: name,
            path,
            group,
            n,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct SlotEntry {
    /// Canonical id — same as path basename.
    pub id: String,
    pub path: PathBuf,
    pub group: Option<String>,
    pub n: u32,
}

/// Parse `{group}-{N}` (grouped) or `slot-{N}` (groupless) into (group, n).
/// None when the name doesn't match the pool's canonical shape.
fn classify(name: &str, cfg: &PoolConfig) -> Option<(Option<String>, u32)> {
    if cfg.groups.is_empty() {
        let n = name.strip_prefix("slot-")?.parse::<u32>().ok()?;
        return Some((None, n));
    }
    for g in &cfg.groups {
        if let Some(rest) = name.strip_prefix(&format!("{g}-"))
            && let Ok(n) = rest.parse::<u32>()
        {
            return Some((Some(g.clone()), n));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_grouped() -> PoolConfig {
        PoolConfig {
            schema_version: 1,
            source: "/x".into(),
            default_commit: "main".into(),
            max_slots: 4,
            groups: vec!["ios".into(), "android".into()],
            submodule_mirror_mode: None,
            submodule_mirror_base: None,
        }
    }

    fn cfg_groupless() -> PoolConfig {
        let mut c = cfg_grouped();
        c.groups.clear();
        c
    }

    #[test]
    fn canonical_id_shapes() {
        assert_eq!(canonical_id(Some("ios"), 0), "ios-0");
        assert_eq!(canonical_id(Some("android"), 15), "android-15");
        assert_eq!(canonical_id(None, 7), "slot-7");
    }

    #[test]
    fn classify_grouped() {
        let c = cfg_grouped();
        assert_eq!(classify("ios-3", &c), Some((Some("ios".into()), 3)));
        assert_eq!(classify("langpack-refactor", &c), None);
        assert_eq!(classify("ios-", &c), None);
        // Unknown group → not canonical for this pool.
        assert_eq!(classify("windows-0", &c), None);
    }

    #[test]
    fn classify_groupless() {
        let c = cfg_groupless();
        assert_eq!(classify("slot-2", &c), Some((None, 2)));
        assert_eq!(classify("ios-0", &c), None);
    }

    #[test]
    fn acquirable_picks_fresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        let v = acquirable(tmp.path(), None, 3, &[]);
        assert_eq!(v.iter().map(|a| (a.n, a.is_fresh)).collect::<Vec<_>>(),
                   vec![(0, true), (1, true), (2, true)]);
    }

    #[test]
    fn acquirable_marks_recycled_not_fresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        // slot-0 idle on disk (.git gitlink resolves, HEAD detached);
        // slot-1 fresh (no dir).
        let slot0 = tmp.path().join("slot-0");
        std::fs::create_dir(&slot0).unwrap();
        let gitdir = tmp.path().join("fake-gitdir");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("HEAD"), "0000000000000000000000000000000000000000\n").unwrap();
        std::fs::write(slot0.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        let v = acquirable(tmp.path(), None, 2, &[]);
        assert_eq!(v.iter().map(|a| (a.n, a.is_fresh)).collect::<Vec<_>>(),
                   vec![(0, false), (1, true)]);
    }

    #[test]
    fn acquirable_skips_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        let slot0 = tmp.path().join("slot-0");
        std::fs::create_dir(&slot0).unwrap();
        let gitdir = tmp.path().join("fake-gitdir");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(slot0.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        // HEAD on a branch = held.
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/my-feature\n").unwrap();

        let v = acquirable(tmp.path(), None, 3, &[]);
        assert_eq!(v.iter().map(|a| a.n).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn is_held_treats_empty_head_as_held() {
        let tmp = tempfile::TempDir::new().unwrap();
        let slot = tmp.path().join("slot-0");
        std::fs::create_dir(&slot).unwrap();
        let gitdir = tmp.path().join("fake-gitdir");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(slot.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        // Empty HEAD = corruption → conservative held (don't stomp).
        std::fs::write(gitdir.join("HEAD"), "").unwrap();
        assert!(is_held_at(&slot));
    }
}
