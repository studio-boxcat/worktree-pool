//! Advisory file-lock mutex via `std::fs::File::try_lock` (stable since 1.89).
//!
//! Lock identity is the open file descriptor; the OS auto-releases on process
//! death (SIGKILL / panic=abort / SIGHUP). No PID tracking, no heartbeat, no
//! staleness threshold — those concerns belong to the kernel.
//!
//! One `FileLock` type, two acquisition disciplines:
//! - **`try_acquire`** (per-slot init mutex): returns `None` on contention so
//!   the caller can try the next slot.
//! - **`acquire`** (pool-wide mutex): blocks via try-loop + sleep, bailing
//!   after `POOL_MAX_WAIT` — loud failure over indefinite hangs.
//!
//! **`is_held`**: liveness probe for crash recovery — opens the file (no
//! create), tries to take the lock, releases immediately. `true` if some
//! process holds the flock; `false` if free or file missing.
use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

const POOL_WAIT_STEP: Duration = Duration::from_millis(100);
const POOL_MAX_WAIT: Duration = Duration::from_secs(60);

pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Try to take an exclusive flock on `path`. Returns `Ok(None)` if another
    /// process holds it; the caller should try the next slot.
    pub fn try_acquire(path: PathBuf) -> Result<Option<Self>> {
        let file = open_for_lock(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("flock {}", path.display()))
            }
        }
    }

    /// Try-loop with sleep, bail after `POOL_MAX_WAIT`. Blocking `lock()` would
    /// be simpler but has no timeout; we want loud failure over indefinite
    /// hangs so operators can investigate.
    pub fn acquire(path: PathBuf) -> Result<Self> {
        let file = open_for_lock(&path)?;
        let mut waited = Duration::ZERO;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if waited >= POOL_MAX_WAIT {
                        bail!(
                            "pool mutex held >{}s at {}; another acquire/release in progress \
                             (legitimate cold submodule clone) or process is stuck. \
                             Inspect with `worktree-pool --pool <key> ls`.",
                            POOL_MAX_WAIT.as_secs(),
                            path.display()
                        );
                    }
                    std::thread::sleep(POOL_WAIT_STEP);
                    waited += POOL_WAIT_STEP;
                }
                Err(TryLockError::Error(e)) => {
                    return Err(e)
                        .with_context(|| format!("flock {}", path.display()));
                }
            }
        }
    }
}

/// True iff some process holds the flock on `path`. False if the file doesn't
/// exist or the lock is free. Used by `admin::unstick` as a read-only
/// diagnostic. Briefly takes and releases the lock when it's free — that's a
/// race window vs. a concurrent acquire that could observe a transient
/// EWOULDBLOCK and try the next slot; unstick is operator-driven and rare so
/// the false-positive is acceptable.
pub fn is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).open(path) else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            // We just took it; release immediately. Drop also releases via
            // close, but explicit unlock is clearer.
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        // I/O error → assume held; safer to skip than to clobber.
        Err(TryLockError::Error(_)) => true,
    }
}

fn open_for_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

// Drop closes File → kernel releases the flock. No explicit unlock needed.
// We don't unlink — would race with concurrent try_acquire's open(O_CREAT):
// A unlinks after lock-release; B opens (creates new inode); C opens the OLD
// path which now maps to B's new inode. B and C would both think they hold
// the flock independently because the lock lives on the inode/OFD. Leaving the
// file around makes existence stable; flock is the only source of truth.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    #[test]
    fn init_mutex_acquire_when_free() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("m.lock");
        let m = FileLock::try_acquire(path).unwrap();
        assert!(m.is_some());
    }

    #[test]
    fn init_mutex_returns_none_when_held() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("m.lock");
        let _held = FileLock::try_acquire(path.clone()).unwrap().unwrap();
        let again = FileLock::try_acquire(path).unwrap();
        assert!(again.is_none());
    }

    #[test]
    fn init_mutex_drop_releases() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("m.lock");
        drop(FileLock::try_acquire(path.clone()).unwrap().unwrap());
        let again = FileLock::try_acquire(path).unwrap();
        assert!(again.is_some());
    }

    #[test]
    fn is_held_true_when_locked() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("m.lock");
        let _held = FileLock::try_acquire(path.clone()).unwrap().unwrap();
        assert!(is_held(&path));
    }

    #[test]
    fn is_held_false_when_unlocked() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("m.lock");
        drop(FileLock::try_acquire(path.clone()).unwrap().unwrap());
        assert!(!is_held(&path));
    }

    #[test]
    fn is_held_false_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("missing.lock");
        assert!(!is_held(&path));
    }

    #[test]
    fn pool_mutex_acquire_when_free() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("p.lock");
        assert!(FileLock::acquire(path).is_ok());
    }

    #[test]
    fn pool_mutex_blocks_then_succeeds_on_release() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("p.lock");
        let held = FileLock::acquire(path.clone()).unwrap();

        let barrier = Arc::new(Barrier::new(2));
        let bc = Arc::clone(&barrier);
        let pc = path.clone();
        let handle = std::thread::spawn(move || {
            bc.wait();
            FileLock::acquire(pc)
        });

        barrier.wait();
        std::thread::sleep(Duration::from_millis(200));
        drop(held);

        let result = handle.join().unwrap();
        assert!(result.is_ok());
    }
}
