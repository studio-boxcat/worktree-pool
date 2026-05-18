//! File-based mutexes used by the tool.
//!
//! - **Init mutex** (per-slot): held only across init/move steps. Created via `O_EXCL`
//!   with a PID; mtime-heartbeated every ~30s during long ops. Reclaim is PID-aware
//!   (immediate when the holder is gone — SIGKILL/panic=abort/cmd+W→SIGHUP), with the
//!   mtime-stale path (>`STALE_AFTER`) kept as a fallback for legacy locks or the
//!   microsecond between create and PID-write.
//! - **Pool-wide release mutex**: serializes the smallest-free-N scan + rename so two
//!   concurrent releases don't race on the same slot id. Same PID + mtime reclaim shape.
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
        match Self::create_excl_with_pid(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // PID-aware reclaim first; mtime fallback (see module header).
                if let Some(pid) = dead_holder_pid(&path) {
                    eprintln!(
                        "warn: reclaiming init mutex {} (holder pid {} no longer running)",
                        path.display(),
                        pid
                    );
                    std::fs::remove_file(&path).ok();
                    Self::create_excl_with_pid(&path)
                        .with_context(|| format!("re-creating mutex {}", path.display()))?;
                } else if age_of(&path)? >= STALE_AFTER {
                    eprintln!(
                        "warn: reclaiming stale init mutex {} (no heartbeat for >{}s)",
                        path.display(),
                        STALE_AFTER.as_secs()
                    );
                    std::fs::remove_file(&path).ok();
                    Self::create_excl_with_pid(&path)
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

    /// O_EXCL create, then write our PID. Contenders use the PID to detect a
    /// dead holder without waiting for the mtime-stale threshold. If the
    /// PID-write fails post-create (rare — disk full mid-write), unlink so we
    /// don't leave an empty file that wedges contenders on the mtime fallback.
    fn create_excl_with_pid(path: &Path) -> std::io::Result<()> {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        if let Err(e) = writeln!(f, "{}", std::process::id()) {
            std::fs::remove_file(path).ok();
            return Err(e);
        }
        Ok(())
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

/// Read a u32 PID from a single-line lock file. None on empty/garbled —
/// callers fall back to mtime-based stale recovery.
fn read_pid_file(path: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

/// Some(pid) if the lock file's PID is parseable and not alive; None otherwise
/// (live holder, legacy/empty file, or the create-then-write-PID race window).
pub fn dead_holder_pid(path: &Path) -> Option<u32> {
    let pid = read_pid_file(path)?;
    (!pid_alive(pid)).then_some(pid)
}

/// Liveness probe via `kill(pid, 0)`. Pool is local-only (single-host invariant),
/// so PID without bootid is sufficient — the only remaining risk is PID reuse,
/// and legitimate pool-mutex holds are sub-second so the reuse window is
/// effectively zero in practice.
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is a probe, not a signal — no side effect on the target.
    // EPERM (process exists, different uid) is treated as alive — a foreign-uid
    // PID collision shouldn't trigger a false reclaim of our own lock file.
    match unsafe { libc::kill(pid as libc::pid_t, 0) } {
        0 => true,
        _ => std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM),
    }
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
/// (same-SHA scan + slot-pick + lock-write + rename) and `release` (pick-target +
/// un-rename). Created via `O_EXCL`; busy-wait with stale-recovery.
pub struct PoolMutex {
    path: PathBuf,
}

/// Pool mutex stale threshold. Legitimate hold is sub-second to a few seconds
/// (scan + worktree_add/reset_hard + lock.write + worktree_rename). Anything older
/// than this is a SIGKILL'd holder leftover and gets reclaimed.
pub const POOL_MUTEX_STALE_AFTER: Duration = Duration::from_secs(120);

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
            match InitMutex::create_excl_with_pid(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // PID-aware reclaim first; mtime fallback (see module header).
                    if let Some(pid) = dead_holder_pid(&path) {
                        eprintln!(
                            "warn: reclaiming pool mutex {} (holder pid {} no longer running)",
                            path.display(),
                            pid
                        );
                        std::fs::remove_file(&path).ok();
                        continue;
                    }
                    if let Ok(age) = age_of(&path)
                        && age >= POOL_MUTEX_STALE_AFTER
                    {
                        eprintln!(
                            "warn: reclaiming stale pool mutex {} (age {}s, threshold {}s) — \
                             prior holder likely crashed",
                            path.display(),
                            age.as_secs(),
                            POOL_MUTEX_STALE_AFTER.as_secs()
                        );
                        std::fs::remove_file(&path).ok();
                        // Loop iterates and tries to create_excl_with_pid again.
                        continue;
                    }
                    if waited >= max_wait {
                        bail!(
                            "pool mutex held >60s at {}; another acquire/release in progress \
                             (legitimate cold submodule clone) or stale holder under threshold. \
                             Inspect with `worktree-pool --pool <key> ls`. Force-clear with \
                             `worktree-pool --pool <key> unstick --pool-mutex` if you're sure \
                             no legitimate holder is running.",
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
