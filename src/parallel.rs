//! Parallel fan-out with inline-fallback when the OS refuses a new thread.
//!
//! `std::thread::Scope::spawn` panics if pthread_create fails (typical trigger:
//! kernel thread-stack exhaustion under swap pressure). Under `panic = "abort"`
//! that aborts the whole process — observed during release-path submodule
//! fan-out on a swap-saturated macOS box (see crash reports
//! 2026-05-10..2026-05-15). These helpers use `Builder::spawn_scoped` and
//! run the closure inline on the caller when spawn returns `Err`, so OS
//! thread starvation degrades parallelism instead of killing the process.

use anyhow::Error;
use std::thread::{Builder, ScopedJoinHandle};

/// Run infallible `f(item)` once per item — in parallel where the OS allows,
/// inline on the caller otherwise.
pub fn for_each<T, F>(items: &[T], f: F)
where
    T: Sync,
    F: Fn(&T) + Sync,
{
    for_each_with(items, f, Builder::new);
}

/// Parallel map: collect `f(&item)` for each item, preserving input order.
/// Inline-fallback on `pthread_create` failure (same rationale as `for_each`).
pub fn map<T, R, F>(items: &[T], f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    map_with(items, f, Builder::new)
}

/// Parallel `Result<()>` fan-out. First `Err` wins; later errors are
/// `eprintln!`-ed (matches the prior in-place collector's behavior).
pub fn try_for_each<T, F>(items: &[T], f: F) -> Result<(), Error>
where
    T: Sync,
    F: Fn(&T) -> Result<(), Error> + Sync,
{
    try_for_each_with(items, f, Builder::new)
}

fn for_each_with<T, F, B>(items: &[T], f: F, mut builder: B)
where
    T: Sync,
    F: Fn(&T) + Sync,
    B: FnMut() -> Builder,
{
    let f = &f;
    std::thread::scope(|scope| {
        for item in items {
            if builder().spawn_scoped(scope, move || f(item)).is_err() {
                f(item);
            }
        }
    });
}

enum Pending<'scope, R> {
    Spawned(ScopedJoinHandle<'scope, R>),
    Inline(R),
}

fn map_with<T, R, F, B>(items: &[T], f: F, mut builder: B) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
    B: FnMut() -> Builder,
{
    let f = &f;
    std::thread::scope(|scope| {
        let mut pending: Vec<Pending<'_, R>> = Vec::with_capacity(items.len());
        for item in items {
            let entry = match builder().spawn_scoped(scope, move || f(item)) {
                Ok(h) => Pending::Spawned(h),
                Err(_) => Pending::Inline(f(item)),
            };
            pending.push(entry);
        }
        pending
            .into_iter()
            .map(|p| match p {
                Pending::Spawned(h) => h
                    .join()
                    .expect("worker thread panicked — unreachable under panic=abort"),
                Pending::Inline(r) => r,
            })
            .collect()
    })
}

fn try_for_each_with<T, F, B>(items: &[T], f: F, builder: B) -> Result<(), Error>
where
    T: Sync,
    F: Fn(&T) -> Result<(), Error> + Sync,
    B: FnMut() -> Builder,
{
    let results = map_with(items, f, builder);

    let mut first_err: Option<Error> = None;
    for r in results {
        if let Err(e) = r {
            if first_err.is_none() {
                first_err = Some(e);
            } else {
                eprintln!("(also) {e:#}");
            }
        }
    }
    first_err.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Deterministically force every `Builder::spawn_scoped` to fail with
    /// `Err`. `pthread_create` rejects `usize::MAX` stack with ENOMEM on
    /// Linux/macOS, surfacing as `io::Error` — no panic, no OS thread is
    /// ever created. Exercises the inline-fallback branch without needing
    /// real memory pressure.
    fn always_fails() -> Builder {
        Builder::new().stack_size(usize::MAX)
    }

    #[test]
    fn for_each_runs_every_item_via_real_spawner() {
        let counter = AtomicUsize::new(0);
        for_each(&[1u32, 2, 3, 4, 5], |x| {
            counter.fetch_add(*x as usize, Ordering::Relaxed);
        });
        assert_eq!(counter.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn for_each_runs_every_item_when_all_spawns_fail() {
        let counter = AtomicUsize::new(0);
        for_each_with(
            &[1u32, 2, 3, 4, 5],
            |x| {
                counter.fetch_add(*x as usize, Ordering::Relaxed);
            },
            always_fails,
        );
        assert_eq!(counter.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn try_for_each_passes_through_ok() {
        let counter = AtomicUsize::new(0);
        let r = try_for_each(&[1u32, 2, 3], |x| {
            counter.fetch_add(*x as usize, Ordering::Relaxed);
            Ok(())
        });
        assert!(r.is_ok());
        assert_eq!(counter.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn try_for_each_returns_first_err_inline_path() {
        let r = try_for_each_with(
            &[1u32, 2, 3],
            |x| {
                if *x == 2 {
                    Err(anyhow::anyhow!("boom {x}"))
                } else {
                    Ok(())
                }
            },
            always_fails,
        );
        let err = r.unwrap_err();
        assert!(err.to_string().contains("boom 2"), "got: {err:#}");
    }

    #[test]
    fn map_preserves_input_order() {
        let v = map(&[1u32, 2, 3, 4], |x| *x * 10);
        assert_eq!(v, vec![10, 20, 30, 40]);
    }

    #[test]
    fn map_preserves_input_order_when_all_spawns_fail() {
        let v = map_with(&[1u32, 2, 3, 4], |x| *x * 10, always_fails);
        assert_eq!(v, vec![10, 20, 30, 40]);
    }

    #[test]
    fn try_for_each_runs_every_item_when_all_spawns_fail() {
        let counter = AtomicUsize::new(0);
        let r = try_for_each_with(
            &[1u32, 2, 3, 4, 5],
            |x| {
                counter.fetch_add(*x as usize, Ordering::Relaxed);
                Ok(())
            },
            always_fails,
        );
        assert!(r.is_ok());
        assert_eq!(counter.load(Ordering::Relaxed), 15);
    }
}
