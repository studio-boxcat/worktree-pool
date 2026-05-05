//! File-based mutexes used by the tool.
//!
//! - **Init mutex** (per-slot): held only across init/move steps. Created via `O_EXCL`;
//!   mtime-heartbeated every ~30s during long ops; reclaimable by another acquire after
//!   `STALE_AFTER` of no heartbeat.
//! - **Pool-wide release mutex**: serializes the smallest-free-N scan + rename so two
//!   concurrent releases don't race on the same slot id.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// Init mutex stale threshold. Slow submodule clones (cold meow-tower) can run
/// 30+ minutes; heartbeat lets us bound legitimate inits well under this.
pub const STALE_AFTER: Duration = Duration::from_secs(60 * 60);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub struct InitMutex {
    path: PathBuf,
    heartbeat_stop: Arc<AtomicBool>,
    heartbeat_thread: Option<std::thread::JoinHandle<()>>,
}

impl InitMutex {
    /// Try to create the mutex via `O_EXCL`. If a stale mutex (mtime > STALE_AFTER) exists,
    /// reclaim it and try again. Otherwise return Ok(None) — operator should try the next slot.
    pub fn try_acquire(path: PathBuf) -> Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        match Self::create_excl(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if Self::is_stale(&path)? {
                    eprintln!(
                        "warn: reclaiming stale init mutex {} (no heartbeat for >{}s)",
                        path.display(),
                        STALE_AFTER.as_secs()
                    );
                    std::fs::remove_file(&path).ok();
                    Self::create_excl(&path)
                        .with_context(|| format!("re-creating mutex {}", path.display()))?;
                } else {
                    return Ok(None);
                }
            }
            Err(e) => {
                return Err(e).with_context(|| format!("creating mutex {}", path.display()));
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            let path = path.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // Bump mtime so other acquires don't classify us as stale.
                    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
                        let _ = f.set_modified(SystemTime::now());
                    }
                    // Sleep in short chunks so Drop responds quickly.
                    let chunks = HEARTBEAT_INTERVAL.as_millis() / 250;
                    for _ in 0..chunks {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            })
        };

        Ok(Some(Self {
            path,
            heartbeat_stop: stop,
            heartbeat_thread: Some(thread),
        }))
    }

    fn create_excl(path: &Path) -> std::io::Result<()> {
        use std::fs::OpenOptions;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(drop)
    }

    fn is_stale(path: &Path) -> Result<bool> {
        Ok(age_of(path)? >= STALE_AFTER)
    }
}

/// File mtime → age (`now - mtime`). Used for stale-mutex detection across the tool.
pub fn age_of(path: &Path) -> Result<Duration> {
    let m = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mtime = m.modified().with_context(|| "no mtime")?;
    Ok(SystemTime::now()
        .duration_since(mtime)
        .unwrap_or(Duration::ZERO))
}

impl Drop for InitMutex {
    fn drop(&mut self) {
        self.heartbeat_stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.heartbeat_thread.take() {
            let _ = t.join();
        }
        // Don't propagate unlink errors out of Drop; release-on-drop is best-effort.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Pool-wide mutex. Held during slot-allocation critical sections in both `acquire`
/// (same-SHA scan + slot-pick + lock-write) and `release` (pick-target + un-rename).
/// Created via `O_EXCL`; busy-wait up to 60s then bail.
pub struct PoolMutex {
    path: PathBuf,
}

impl PoolMutex {
    pub fn acquire(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        let mut waited = Duration::ZERO;
        let max_wait = Duration::from_secs(60);
        let step = Duration::from_millis(100);
        loop {
            match InitMutex::create_excl(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if waited >= max_wait {
                        bail!(
                            "pool mutex held >60s at {}; another acquire/release stuck (try `unstick`)",
                            path.display()
                        );
                    }
                    std::thread::sleep(step);
                    waited += step;
                }
                Err(e) => {
                    return Err(e).context(format!("creating pool mutex {}", path.display()));
                }
            }
        }
    }
}

impl Drop for PoolMutex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
