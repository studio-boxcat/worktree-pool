//! Slot identity, enumeration, and idle-slot picking.
//!
//! A slot is **always** at its canonical path `<pool>/{group}-{N}` (or
//! `<pool>/slot-{N}` for groupless pools). It's **held** iff the lock file at
//! its gitdir exists; **idle** otherwise. The user-given name lives in the
//! branch ref inside the slot, not on disk — see [[../docs/lifecycle.md]].
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::config::PoolConfig;
use crate::{fs_paths, git};

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
        Some(g) => {
            if !cfg.groups.iter().any(|x| x == g) {
                bail!(
                    "unknown group '{g}'; pool's groups: {}",
                    cfg.groups.join(", ")
                );
            }
            Ok(Some(cfg.groups.iter().find(|x| x.as_str() == g).unwrap().as_str()))
        }
        None => {
            let first = cfg.groups[0].as_str();
            eprintln!("--group not given; defaulting to '{first}' (first of: {})", cfg.groups.join(", "));
            Ok(Some(first))
        }
    }
}

/// Iterator-friendly enumeration of canonical slot Ns currently *acquirable*:
/// either `<pool>/{canonical_id(group, N)}` doesn't exist (fresh) OR it exists but has no
/// held-marker at its gitdir (recycled idle).
///
/// Acquire iterates this in order and tries the init mutex on each; the first mutex it
/// successfully creates is its slot.
///
/// **Fresh vs recycled in over-provisioned pools.** Fresh creation is bounded by
/// `0..max_slots` (never grow the pool past max_slots). Recycled-idle dirs at
/// N >= max_slots can exist (max_slots was reduced after materialization) and
/// are surfaced here as acquirable — reusing them eats down the surplus.
pub fn acquirable_ns(
    pool_root: &Path,
    group: Option<&str>,
    max_slots: u32,
    entries: &[SlotEntry],
) -> Result<Vec<u32>> {
    let mut out: Vec<u32> = Vec::new();
    for n in 0..max_slots {
        let id = canonical_id(group, n);
        let p = pool_root.join(&id);
        if !p.exists() {
            out.push(n); // fresh — will `worktree add`
            continue;
        }
        if !is_held_at(&p) {
            out.push(n); // recycled idle — will `reset --hard`
        }
    }
    for entry in entries {
        if entry.group.as_deref() == group
            && entry.n >= max_slots
            && !is_held_at(&entry.path)
        {
            out.push(entry.n);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// True iff the slot dir at `path` has a held marker at its gitdir.
/// Returns `true` on gitdir resolution failure — refuses to stomp on a dir
/// whose state we can't read. False on missing lock.
pub fn is_held_at(path: &Path) -> bool {
    match git::worktree_gitdir(path) {
        Ok(gd) => fs_paths::slot_lock(&gd).exists(),
        Err(_) => true,
    }
}

/// Count held slots in `group` — canonical slots with a lock at their gitdir.
pub fn count_held_in_group(
    pool_root: &Path,
    cfg: &PoolConfig,
    group: Option<&str>,
) -> Result<usize> {
    Ok(enumerate(pool_root, cfg)?
        .into_iter()
        .filter(|e| e.group.as_deref() == group && is_held_at(&e.path))
        .count())
}

/// Look up a held slot by branch name. Scans canonical slots with locks,
/// matches by `git symbolic-ref --short HEAD`. None if no held slot has
/// branch `name` (already released, never acquired, or HEAD detached).
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
    fn acquirable_ns_picks_fresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        let v = acquirable_ns(tmp.path(), None, 3, &[]).unwrap();
        assert_eq!(v, vec![0, 1, 2]);
    }

    #[test]
    fn acquirable_ns_skips_held() {
        // A slot dir with a placeholder gitdir + lock at fs_paths location.
        let tmp = tempfile::TempDir::new().unwrap();
        let slot0 = tmp.path().join("slot-0");
        std::fs::create_dir(&slot0).unwrap();
        // Plant a .git gitlink pointing at a fake gitdir we can write a lock under.
        let gitdir = tmp.path().join("fake-gitdir");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(slot0.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::create_dir_all(gitdir.join("worktree-pool")).unwrap();
        std::fs::write(gitdir.join("worktree-pool").join("lock"), "started_at: 2026-01-01T00:00:00Z\nfull_sha: 0000000000000000000000000000000000000000\n").unwrap();

        let v = acquirable_ns(tmp.path(), None, 3, &[]).unwrap();
        assert_eq!(v, vec![1, 2]); // slot-0 is held, skipped
    }
}
