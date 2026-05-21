# TODO

Deferred work for worktree-pool. See `CLAUDE.md` for the design contract.

## API / contract

- **Machine-readable acquire output (already locked).** `acquire`'s stdout is exactly one line — the canonical slot path — and the contract is documented in `src/cli.rs` and asserted indirectly via smoke tests. The `--print-path-only` flag this entry previously proposed is moot: current behavior already meets the spec. (Kept as a note for archaeologists.)

## `wt land` follow-ups

- **Direct integration test for submodule ancestry preflight.** Test setup requires the source's submodule clone to diverge from its tracked gitlink without tripping `M sub` in the parent's `status --porcelain --ignore-submodules=untracked` (the main_dirty check at bin/wt:651). `submodule.sub.ignore=all` silences status but is overridden by the explicit `--ignore-submodules=untracked` flag. `extensions.worktreeConfig=true` + per-worktree config doesn't help for the same reason. Probable solution: change land to honor per-worktree config (drop the CLI flag, rely on operator config), then a test can opt-out via `git config --worktree submodule.sub.ignore all`. Currently preflight is exercised implicitly by other land tests not failing.

## Test coverage gaps (deferred)

- **Nested-submodule recursion** — the parallel-recursion-inside-parallel path (highest-impact perf win) isn't tested end-to-end. Extend `make_fixture_with_tagged_submodules` with a second-level `.gitmodules` inside a sub-staging.
- **`ExitKind::Contended` (exit code 3)** — code 4 (Capacity) and 5 (UniqueSha) are tested; 3 needs a separate-process flock holder against the init-mutex path. Mid-priority — 4/5 cover the high-impact cases.

## Won't-do (decided this session)

- **Migrate read ops to `gix` or `git2`** — Profiled (0981f5c, f22a5cf). Both add 1-2 MB binary; the spawn-savings (~10 ms / acquire) are dwarfed by network-bound submodule clones. Shell-out + `opt-level = "s"` is the local optimum at 669 KB binary.
- **Consumer-supplied submodule URL rewrites via callback** — current pool-config-driven `submodule_mirror_mode + submodule_mirror_base` covers the two consumer patterns (bare-mirror, git-modules); a plugin shape isn't justified by 2 consumers.
- **`rayon` for parallel submodule update / scans** — we use `parallel::{for_each, try_for_each, map}` (in-house wrapper over `std::thread::scope` + `Builder::spawn_scoped` with inline-fallback) for submodule update + same-SHA scan + ls --git-status row population: N is small (≤max-slots, ≤submodule-count), thread-spawn cost is negligible compared to git-process spawn, and `rayon` adds ~50KB binary + a transitive scheduler we don't need. The wrapper exists because raw `Scope::spawn` panics on OS thread-create failure under `panic = "abort"` (verified crash class, May 2026).
- **Port `wt land`'s `land.lock` from bash PID+mtime to flock** — macOS lacks `flock(1)`; the bash impl is ~40 LOC of clear code with equivalent semantics. Not worth the platform-portability hit.
- **Extract `_land_sync_sub` (full helper)** — the slot→main and main→slot blocks share buffering scaffolding + the attach-priority loop (now `_land_attach_first`), but the differences (fetch direction, fail policy: die vs WARN, post-ff message style) parameterize messily in bash. Saved ~15 LOC via the attach helper; the rest stays inlined for readability.
