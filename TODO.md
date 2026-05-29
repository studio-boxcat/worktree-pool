# TODO

Deferred work for worktree-pool. See `CLAUDE.md` for the design contract.

- **Domain newtypes for slot id / branch / group / full SHA** *(needs confirmation)* —
  these recur as raw `String` across `acquire`, `release`, `slot`, `dashboard`,
  `config`. Newtypes (`SlotId`, `BranchName`, `GroupName`, `FullSha`) would catch
  mix-ups at compile time. Deferred because `groups` crosses the YAML config
  serialization boundary (`config.rs`) — needs a round-trip test to confirm the
  on-disk format is unchanged before adopting. Flagged by the 2026-05-29 audit.

## Won't-do (decided this session)

- **Migrate read ops to `gix` or `git2`** — Profiled (0981f5c, f22a5cf). Both add 1-2 MB binary; the spawn-savings (~10 ms / acquire) are dwarfed by network-bound submodule clones. Shell-out + `opt-level = "s"` is the local optimum at 669 KB binary.
- **Consumer-supplied submodule URL rewrites via callback** — current pool-config-driven `submodule_mirror_mode + submodule_mirror_base` covers the two consumer patterns (bare-mirror, git-modules); a plugin shape isn't justified by 2 consumers.
- **`rayon` for parallel submodule update / scans** — we use `parallel::{for_each, try_for_each, map}` (in-house wrapper over `std::thread::scope` + `Builder::spawn_scoped` with inline-fallback) for submodule update + same-SHA scan + ls --git-status row population: N is small (≤max-slots, ≤submodule-count), thread-spawn cost is negligible compared to git-process spawn, and `rayon` adds ~50KB binary + a transitive scheduler we don't need. The wrapper exists because raw `Scope::spawn` panics on OS thread-create failure under `panic = "abort"` (verified crash class, May 2026).
- **Port `wt land`'s `land.lock` from bash PID+mtime to flock** — macOS lacks `flock(1)`; the bash impl is ~40 LOC of clear code with equivalent semantics. Not worth the platform-portability hit.
- **Extract `_land_sync_sub` (full helper)** — the slot→main and main→slot blocks share buffering scaffolding + the attach-priority loop (now `_land_attach_first`), but the differences (fetch direction, fail policy: die vs WARN, post-ff message style) parameterize messily in bash. Saved ~15 LOC via the attach helper; the rest stays inlined for readability.
- **`--print-path-only` flag for `acquire`** — moot: `acquire`'s stdout is already exactly one line (the canonical slot path), documented in `src/cli.rs` and in docs/cli.md. No flag needed.
