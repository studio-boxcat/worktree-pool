//! Slot identity, enumeration, and idle-slot picking.
//!
//! A slot has two on-disk states:
//! - **Idle**: directory at `<pool>/{group}-{N}` (or `<pool>/slot-{N}` for groupless pools).
//!   No lock file at its gitdir.
//! - **Held**: directory at `<pool>/<name>` (renamed from canonical at acquire).
//!   Lock file present at `<gitdir>/worktree-pool/lock`.
//!
//! Transient post-crash states are reconciled by `release::reclaim_stale`.
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

/// Find the smallest N in `0..max_slots` such that `<pool>/{canonical_id(group, N)}`
/// does not exist on disk and is not used as the current name of any held slot.
/// Used by `release` to pick the un-rename target; doesn't consider lock state because
/// release-mutex serializes the call.
pub fn smallest_free_n(
    pool_root: &Path,
    group: Option<&str>,
    max_slots: u32,
    held_names: &[String],
) -> Result<u32> {
    for n in 0..max_slots {
        let id = canonical_id(group, n);
        let p = pool_root.join(&id);
        // Free if dir doesn't exist AND no held slot is currently using this name
        // (could happen if a previous release renamed back to {group}-N and crashed).
        if !p.exists() && !held_names.iter().any(|h| h == &id) {
            return Ok(n);
        }
    }
    bail!(
        "no free slot in {}/{:?}: all {max_slots} are in use",
        pool_root.display(),
        group
    )
}

/// Iterator-friendly enumeration of canonical slot Ns that are currently *acquirable*:
/// either `<pool>/{canonical_id(group, N)}` doesn't exist (fresh) OR it exists but has no
/// held-marker at its gitdir (recycled idle). Skips Ns whose canonical name is currently
/// used by a renamed (held) slot — name collision avoidance for `acquire --name {group}-N`.
///
/// Acquire iterates this in order and tries the init mutex on each; the first mutex it
/// successfully creates is its slot. Lets two parallel acquires fall through to different
/// Ns without the spurious "mutex contended" error.
pub fn acquirable_ns(
    pool_root: &Path,
    group: Option<&str>,
    max_slots: u32,
    renamed_held: &[String],
) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for n in 0..max_slots {
        let id = canonical_id(group, n);
        if renamed_held.iter().any(|h| h == &id) {
            continue;
        }
        let p = pool_root.join(&id);
        if !p.exists() {
            out.push(n); // fresh — will `worktree add`
            continue;
        }
        // Dir exists. Held iff there's a lock at the worktree's gitdir; idle otherwise.
        let held = match git::worktree_gitdir(&p) {
            Ok(gd) => fs_paths::slot_lock(&gd).exists(),
            Err(_) => true, // can't resolve gitdir → treat as held (don't stomp)
        };
        if !held {
            out.push(n); // recycled idle — will `reset --hard`
        }
    }
    Ok(out)
}

/// Count slots occupying capacity in `requested_group`. A slot is "occupying"
/// if its dir is `Renamed` (home N hidden, takes one slot of capacity). Held
/// slots count toward their lock's group; zombies (Renamed, no/unparseable lock)
/// have unknown group and count toward EVERY group — conservative but loud, so
/// over-provisioning surfaces as a capacity error instead of silently growing
/// the pool past `max_slots`. (`reclaim_stale` runs first under the same mutex
/// and tries to clear zombies; only the unrecoverable ones reach this count.)
pub fn count_occupying_in_group(
    pool_root: &Path,
    cfg: &PoolConfig,
    requested_group: Option<&str>,
) -> Result<usize> {
    let mut count = 0;
    for entry in enumerate(pool_root, cfg)? {
        if !matches!(entry.kind, SlotEntryKind::Renamed) {
            continue;
        }
        let Ok(gitdir) = git::worktree_gitdir(&entry.path) else {
            continue;
        };
        let lock_path = fs_paths::slot_lock(&gitdir);
        if !lock_path.exists() {
            count += 1;
            continue;
        }
        match crate::lock::Lock::read(&lock_path) {
            Ok(lock) if lock.group.as_deref() == requested_group => count += 1,
            Err(_) => count += 1,
            _ => {}
        }
    }
    Ok(count)
}

/// Enumerate top-level entries of the pool dir, partitioned by whether they look
/// like canonical idle ids (`{group}-{N}` or `slot-{N}`) vs held names.
/// Skips `.meta` and any entry not a directory.
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
        let kind = classify(&name, cfg);
        out.push(SlotEntry { name, path, kind });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct SlotEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: SlotEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotEntryKind {
    /// Looks like `{group}-{N}` for one of the configured groups, or `slot-{N}` if no groups.
    Canonical { group: Option<String>, n: u32 },
    /// Doesn't match the canonical pattern — likely a renamed (held) slot.
    Renamed,
}

fn classify(name: &str, cfg: &PoolConfig) -> SlotEntryKind {
    if cfg.groups.is_empty() {
        if let Some(rest) = name.strip_prefix("slot-")
            && let Ok(n) = rest.parse::<u32>()
        {
            return SlotEntryKind::Canonical { group: None, n };
        }
        return SlotEntryKind::Renamed;
    }
    for g in &cfg.groups {
        if let Some(rest) = name.strip_prefix(&format!("{g}-"))
            && let Ok(n) = rest.parse::<u32>()
        {
            return SlotEntryKind::Canonical {
                group: Some(g.clone()),
                n,
            };
        }
    }
    SlotEntryKind::Renamed
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
        assert_eq!(
            classify("ios-3", &c),
            SlotEntryKind::Canonical {
                group: Some("ios".into()),
                n: 3
            }
        );
        assert_eq!(classify("langpack-refactor", &c), SlotEntryKind::Renamed);
        assert_eq!(classify("ios-", &c), SlotEntryKind::Renamed);
        // Unknown group → renamed (not canonical for this pool).
        assert_eq!(classify("windows-0", &c), SlotEntryKind::Renamed);
    }

    #[test]
    fn classify_groupless() {
        let c = cfg_groupless();
        assert_eq!(
            classify("slot-2", &c),
            SlotEntryKind::Canonical { group: None, n: 2 }
        );
        assert_eq!(classify("ios-0", &c), SlotEntryKind::Renamed);
    }

    #[test]
    fn smallest_free_n_picks_lowest() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("ios-0")).unwrap();
        std::fs::create_dir(tmp.path().join("ios-2")).unwrap();
        let n = smallest_free_n(tmp.path(), Some("ios"), 4, &[]).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn smallest_free_n_avoids_held_names_at_canonical() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No dirs exist, but `ios-0` is in the held list (mid-rename, etc.).
        let n = smallest_free_n(tmp.path(), Some("ios"), 4, &["ios-0".into()]).unwrap();
        assert_eq!(n, 1);
    }
}
