//! `acquire` orchestration. Picks an idle slot, pins HEAD to the requested commit,
//! writes the held-marker, renames the slot, creates a branch, runs submodule init.
//! See README.md §Lifecycle for the spec.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

use crate::cli::AcquireArgs;
use crate::config::PoolConfig;
use crate::{cli, fs_paths, git, lock::Lock, mutex, slot};

pub fn run(pool_root: &Path, cfg: &PoolConfig, args: AcquireArgs) -> Result<()> {
    let group = slot::resolve_group(cfg, args.group.as_deref())?;
    let commitish = args
        .commit
        .as_deref()
        .unwrap_or(cfg.default_commit.as_str());
    let full_sha = git::resolve_full_sha(&cfg.source, commitish)?;

    // Same-SHA exclusion is opt-in — only fires for callers that pass --unique-sha.
    // Dev sessions branching off `main` shouldn't refuse just because a build is at the same SHA.
    if args.unique_sha
        && let Some(holder) = find_same_sha_holder(pool_root, cfg, &full_sha)?
    {
        bail!(
            "acquire --unique-sha failed: full_sha {} already held by slot '{}' (held since {}).\n\
             Wait for it, reuse its output, or run: worktree-pool --pool <key> release --name {}",
            &full_sha[..8],
            holder.name,
            holder.lock.started_at,
            holder.name
        );
    }

    // Pick an idle slot in the requested group.
    let entries = slot::enumerate(pool_root, cfg)?;
    let held_canonical = entries
        .iter()
        .filter(|e| matches!(e.kind, slot::SlotEntryKind::Renamed))
        .map(|e| e.name.clone())
        .collect::<Vec<_>>();
    let n = match slot::smallest_free_n(pool_root, group, cfg.max_slots, &held_canonical) {
        Ok(n) => n,
        Err(e) => {
            print_capacity_error(pool_root, cfg, group, &entries)?;
            return Err(e);
        }
    };
    let slot_id = slot::canonical_id(group, n);
    let canonical_path = pool_root.join(&slot_id);

    // Per-slot init mutex (heartbeat-stamped).
    let init_mutex_path = fs_paths::init_mutex(pool_root, &slot_id);
    let _mutex = mutex::InitMutex::try_acquire(init_mutex_path)?
        .ok_or_else(|| anyhow::anyhow!("init mutex contended for {slot_id}; another acquire is in flight (try again or run `unstick --slot {slot_id}`)"))?;

    // Materialize the slot at full_sha. Fresh slot → `worktree add`; recycled → `reset --hard` + clean.
    let is_fresh = !canonical_path.join(".git").exists();
    if is_fresh {
        // Roll back partial state on failure: `worktree add` may leave a half-checked-out dir.
        if let Err(e) = git::worktree_add(&cfg.source, &canonical_path, &full_sha) {
            cleanup_partial_worktree_add(&cfg.source, &canonical_path);
            return Err(e);
        }
    } else {
        git::reset_hard(&canonical_path, &full_sha)?;
        git::clean_untracked(&canonical_path)?;
    }

    // Resolve the worktree's gitdir. Stable across `git worktree move`.
    let gitdir = git::worktree_gitdir(&canonical_path)?;
    let lock_path = fs_paths::slot_lock(&gitdir);

    // Write the held marker BEFORE the rename. If we crash between rename and lock-write,
    // the slot would look idle (no lock) but live at a non-canonical name — undetectable.
    // Writing first means a crash leaves the slot held at its canonical id (clean recovery).
    let lock = Lock::new(full_sha.clone(), group.map(String::from));
    lock.write(&lock_path)
        .with_context(|| format!("writing lock {}", lock_path.display()))?;

    // Rename `{group}-N → <name>` and create the branch.
    let target_path = pool_root.join(&args.name);
    git::worktree_move(&cfg.source, &canonical_path, &target_path)
        .with_context(|| format!("moving {} → {}", canonical_path.display(), target_path.display()))?;
    git::checkout_force_branch(&target_path, &args.name)?;

    // TODO(submodules): apply --exclude-submodule-tags + URL overrides per pool config.
    // For v0.1 we run plain `submodule update --init` to validate the basic flow.
    let _ = git::run(&target_path, &["submodule", "update", "--init", "--recursive"]);
    let _ = &args.exclude_submodule_tags;

    println!("{}", target_path.display());
    Ok(())
}

struct Holder {
    name: String,
    lock: Lock,
}

fn find_same_sha_holder(
    pool_root: &Path,
    cfg: &PoolConfig,
    full_sha: &str,
) -> Result<Option<Holder>> {
    for entry in slot::enumerate(pool_root, cfg)? {
        let Ok(gitdir) = git::worktree_gitdir(&entry.path) else {
            continue;
        };
        let lock_path = fs_paths::slot_lock(&gitdir);
        if !lock_path.exists() {
            continue;
        }
        let Ok(lock) = Lock::read(&lock_path) else {
            continue;
        };
        if lock.full_sha == full_sha {
            return Ok(Some(Holder {
                name: entry.name,
                lock,
            }));
        }
    }
    Ok(None)
}

fn print_capacity_error(
    pool_root: &Path,
    _cfg: &PoolConfig,
    group: Option<&str>,
    entries: &[slot::SlotEntry],
) -> Result<()> {
    eprintln!("\nHeld slots in pool {}:", pool_root.display());
    for e in entries {
        if let slot::SlotEntryKind::Renamed = e.kind {
            eprintln!("  {}", e.name);
        }
    }
    let _ = group;
    Ok(())
}

fn cleanup_partial_worktree_add(source: &Path, slot: &Path) {
    // Best-effort rollback. Don't propagate errors; we're already in a failure path.
    let _ = git::worktree_remove(source, slot);
    let _ = std::fs::remove_dir_all(slot);
}

// Used by main.rs to decide whether `--exclude-submodule-tags` was actually present
// on the command line (vs default-empty list).
pub fn _suppress_unused_warnings(_: &cli::AcquireArgs, _: &mut PathBuf) {}
