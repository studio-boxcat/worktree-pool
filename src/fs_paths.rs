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

/// Per-source mutex serializing top-level submodule.<name>.url writes that
/// target the shared `<source>/.git/config`. Lives at
/// `<source-gitdir>/worktree-pool-config.lock`, so parallel acquires across
/// pools sharing the same source repo don't fight on the lockfile.
pub fn source_config_mutex(source_gitdir: &Path) -> PathBuf {
    source_gitdir.join("worktree-pool-config.lock")
}

/// Git's per-worktree index lock at `<worktree_gitdir>/index.lock`. Foreign to
/// worktree-pool — git owns the lifecycle — but the file can leak when a git
/// process dies between `open(O_CREAT|O_EXCL)` and a write (signal, panic,
/// untracked-cache writeback aborted mid-flight). `acquire` removes it before
/// recycling a slot (the slot is idle under the pool mutex, so the remove is
/// race-free) — otherwise the recycled `reset --hard` would fail with EEXIST.
pub(crate) fn worktree_index_lock(worktree_gitdir: &Path) -> PathBuf {
    worktree_gitdir.join("index.lock")
}

/// Iterate every initialized pool dir under `$WORKTREE_ROOT` (entries that
/// contain a `.meta/config.yaml`). Silent on missing root or `read_dir`
/// failures — callers treat "no pools" as the empty case.
pub fn for_each_pool_dir(mut f: impl FnMut(PathBuf)) {
    let Ok(rd) = std::fs::read_dir(worktree_root()) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if pool_config(&path).exists() {
            f(path);
        }
    }
}

/// `$WORKTREE_ROOT`. Panics with a clear message if the var is unset — every
/// pool path is anchored here, so silent fallback would mis-locate state.
pub fn worktree_root() -> PathBuf {
    std::env::var_os("WORKTREE_ROOT")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .expect("WORKTREE_ROOT is unset; add `export WORKTREE_ROOT=\"$HOME/.worktree-pool\"` to ~/.zshenv.local (or equivalent)")
}
