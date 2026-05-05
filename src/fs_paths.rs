//! All on-disk paths the tool touches. Single source of truth; no ad-hoc joins
//! at call sites.
use std::path::{Path, PathBuf};

/// Pool root: `~/.worktree-pool/<key>/`. No env or registry — convention.
pub fn pool_root(key: &str) -> PathBuf {
    home().join(".worktree-pool").join(key)
}

/// `<pool>/.meta/config.yaml`.
pub fn pool_config(pool_root: &Path) -> PathBuf {
    pool_root.join(".meta/config.yaml")
}

/// Per-slot init mutex: `<pool>/.meta/init/<slot-id>.lock`.
pub fn init_mutex(pool_root: &Path, slot_id: &str) -> PathBuf {
    pool_root.join(".meta/init").join(format!("{slot_id}.lock"))
}

/// Pool-wide release mutex: `<pool>/.meta/release.lock`.
pub fn release_mutex(pool_root: &Path) -> PathBuf {
    pool_root.join(".meta/release.lock")
}

/// Lock file inside the source repo's per-worktree gitdir.
/// Caller passes `<source>/.git/worktrees/<id>/`.
pub fn slot_lock(worktree_gitdir: &Path) -> PathBuf {
    worktree_gitdir.join("worktree-pool/lock")
}

/// `~/`. Panics if `$HOME` is unset (we don't ship to environments where that's plausible).
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME is unset; worktree-pool requires a user home directory")
}
