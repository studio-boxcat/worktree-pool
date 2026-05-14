//! All on-disk paths the tool touches. Single source of truth; no ad-hoc joins
//! at call sites.
use std::path::{Path, PathBuf};

/// Pool root directory: `$WORKTREE_ROOT/<key>/`. `WORKTREE_ROOT` is required —
/// no fallback. Set it in `~/.zshenv.local` (or equivalent) per host.
pub fn pool_root(key: &str) -> PathBuf {
    worktree_root().join(key)
}

/// `<pool>/.meta/config.yaml`.
pub fn pool_config(pool_root: &Path) -> PathBuf {
    pool_root.join(".meta/config.yaml")
}

/// Per-slot init mutex: `<pool>/.meta/init/<slot-id>.lock`.
pub fn init_mutex(pool_root: &Path, slot_id: &str) -> PathBuf {
    pool_root.join(".meta/init").join(format!("{slot_id}.lock"))
}

/// Pool-wide mutex serializing the slot-allocation critical sections (acquire's
/// scan-and-pick window AND release's pick-target window): `<pool>/.meta/pool.lock`.
pub fn pool_mutex(pool_root: &Path) -> PathBuf {
    pool_root.join(".meta/pool.lock")
}

/// Lock file inside the source repo's per-worktree gitdir.
/// Caller passes `<source>/.git/worktrees/<id>/`.
pub fn slot_lock(worktree_gitdir: &Path) -> PathBuf {
    worktree_gitdir.join("worktree-pool/lock")
}

/// Sticky pool-ownership marker, sibling to `slot_lock`. Written once at
/// `acquire` time, never deleted by `release`/`reclaim_stale`/cleanup. Its
/// presence is the positive signal that the slot dir was ever pool-acquired
/// — without it, `reclaim_stale` would silently absorb manually-`git worktree
/// add`-ed dirs and destructively un-rename them. See `release::reclaim_stale`.
pub fn slot_created_marker(worktree_gitdir: &Path) -> PathBuf {
    worktree_gitdir.join("worktree-pool/created")
}

/// `$WORKTREE_ROOT`. Panics with a clear message if the var is unset — every
/// pool path is anchored here, so silent fallback would mis-locate state.
pub fn worktree_root() -> PathBuf {
    std::env::var_os("WORKTREE_ROOT")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .expect("WORKTREE_ROOT is unset; add `export WORKTREE_ROOT=\"$HOME/.worktree-pool\"` to ~/.zshenv.local (or equivalent)")
}
