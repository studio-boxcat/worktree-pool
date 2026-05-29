# TODO

Deferred work for worktree-pool. See `CLAUDE.md` for the design contract.

(Empty — the domain-newtype item shipped: `src/types.rs` defines `SlotId`,
`BranchName`, `GroupName`, `FullSha`, threaded through the modules; the config
round-trip tests confirm the on-disk `groups` format is unchanged.)

## Won't-do (decided this session)

- **Migrate read ops to `gix` or `git2`** — Profiled (0981f5c, f22a5cf). Both add 1-2 MB binary; the spawn-savings (~10 ms / acquire) are dwarfed by network-bound submodule clones. Shell-out + `opt-level = "s"` is the local optimum at 669 KB binary.
- **Consumer-supplied submodule URL rewrites via callback** — current pool-config-driven `submodule_mirror_mode + submodule_mirror_base` covers the two consumer patterns (bare-mirror, git-modules); a plugin shape isn't justified by 2 consumers.
- **`rayon` for parallel submodule update / scans** — we use `parallel::{for_each, try_for_each, map}` (in-house wrapper over `std::thread::scope` + `Builder::spawn_scoped` with inline-fallback) for submodule update + same-SHA scan + ls --git-status row population: N is small (≤max-slots, ≤submodule-count), thread-spawn cost is negligible compared to git-process spawn, and `rayon` adds ~50KB binary + a transitive scheduler we don't need. The wrapper exists because raw `Scope::spawn` panics on OS thread-create failure under `panic = "abort"` (verified crash class, May 2026).
- **Port `wt land`'s `land.lock` from bash PID+mtime to flock** — macOS lacks `flock(1)`; the bash impl is ~40 LOC of clear code with equivalent semantics. Not worth the platform-portability hit.
- **`--print-path-only` flag for `acquire`** — moot: `acquire`'s stdout is already exactly one line (the canonical slot path), documented in `src/cli.rs` and in docs/cli.md. No flag needed.
