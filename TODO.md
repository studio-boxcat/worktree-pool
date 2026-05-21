# TODO

Deferred work for worktree-pool. See `CLAUDE.md` for the design contract.

## Efficiency

Surfaced by efficiency-review agents during the v0.1.1 profile pass. Rust paths are sub-µs (yaml parse 269 ns); the cost is dominated by git subprocess spawns. The ones below are the biggest unrealized wins.

- **Parallel submodule recursion** (`src/submodules.rs:update_recursive`) — top-level submodules already update in parallel via `crate::parallel::try_for_each`, but recursion into each child still runs sequentially per top-level. Spawn-per-child at the recursion boundary (reuse `parallel::*`, not rayon — see Won't-do entry). For Unity-Packages with K nested `.gitmodules`, up to Kx on cold-clone acquire (the multi-minute path that dominates real-world use). **Highest-impact win.**
- **Parallel `--git-status` row population** (`src/dashboard.rs::augment_with_git`) — sequential `git status --porcelain` + `rev-list --count` per held slot. With H held slots, that's 2H sequential spawns. `rayon::par_iter_mut` over rows. ~min(H, ncpu)x speedup; for H=8 likely ~5x. Adds `rayon` dep (~50KB to binary).
- **Parallel same-SHA scan** (`src/acquire.rs::find_same_sha_holder`) — sequential `worktree_gitdir` + lock-read per entry. Under pool mutex, blocks all other acquires. Parallelize with rayon. For pool of 8: ~5x reduction in scan time.
- **Dedupe `slot::enumerate` calls per `acquire`** — currently `find_same_sha_holder`, `count_held_in_group`, and `acquirable_ns` each call `enumerate` separately → 3 pool-dir reads per acquire. Pass an `&[SlotEntry]` once. Minor savings (each call is fast post-`worktree_gitdir` optimization), but cleaner.
- **Drop `is_fresh` re-stat in `acquire::run`** — `is_fresh = !canonical_path.join(".git").exists()` redundant with `acquirable_ns` classification. Have `acquirable_ns` return `(n, is_fresh)`. Removes one stat + the TOCTOU surface.

## API / contract

- **Machine-readable acquire output** — currently `acquire`'s last stdout line is the slot path, callers parse with `tail -1` (justfile) / `.split('\n').pop()` (TS). Brittle if a future trailing log line slips into stdout. Consider `--print-path-only` flag (only the path, no logs to stdout) or `--format json` (JSON-on-stdout). Either tightens the consumer contract; current `tail -1` works but is load-bearing.
- **Acquire / release exit-code distinguishability** — callers can't tell apart "init mutex contended (transient, retry)" from "all slots held (capacity)" from "same-SHA exclusion fired (don't retry, real conflict)". A retry-aware caller would benefit from distinct exit codes. Document the categories first; then assign.

## Doc / discoverability

- **`worktree-pool doctor` could check pool initialization** — currently host-level checks only (arch, git, `$WORKTREE_ROOT` dir presence, quarantine xattr). Could enumerate registered pools (subdirs of `$WORKTREE_ROOT/` with a `.meta/config.yaml`) and validate each (config schema, source path exists, etc.).
- **Bench fixture is too small** — `scripts/bench-fixture.sh` makes a bare repo with one commit, no submodules. Real cold-acquire cost (the dominant wall time) isn't measured. A heavier fixture with a few mock submodules would expose the parallel-submodule-recursion win when implemented.

## Lint

- **`clippy::type-complexity` in `src/submodules.rs:51`** — pre-existing; `HashMap<String, (Option<String>, Option<String>, Vec<String>)>` triggers `-D warnings`. Hoist into a `type ByName = ...` alias or restructure into a struct.

## `wt land` follow-ups

- **Precheck refuses on any submodule-internal `M` status.** `git -C <main_path> status --porcelain | awk '!/^\?\?/'` shows ` M sub` whenever a submodule has untracked / dirty content (default git config), blocking land even when the submodule has no commit work to merge. Operators silence per-submodule via `git config submodule.<name>.ignore untracked` (the test does this), but a friendlier default would be `git status --ignore-submodules=untracked` in the precheck — matches what the ff-only safety net already protects against.
- **`wt land`'s output interleaves under parallel submodule propagation.** Acceptable for now; revisit if operators complain. Could buffer stdout per subshell into `mktemp` and concatenate after `wait`.
- **`cmd_land` reuse: collapse repeated `echo "land: ..." >&2; exit 1` pattern.** ~9 sites in `bin/wt`. A `warn_land() { echo "land: $*" >&2; }` helper (and tighter `die "land: ..."` calls) would dedupe. Pre-existing; surfaced by simplify-agent during the verb-rename audit.
- **`cmd_land` reuse: factor `git -c core.hooksPath=/dev/null`.** Repeated 3× in `bin/wt` (`bin/wt:464, 487, 502`). A `git_no_hooks()` wrapper would dedupe. Pre-existing; surfaced by simplify-agent during the verb-rename audit.
- **`cmd_land` reuse: extract submodule fetch+attach+ff helper.** The slot→main propagation block (bin/wt:436-477) and the slot-direction refresh block (bin/wt:498-572) share ~80% structure: `awk '$2=="160000"'` filter, `[ -e .../.git ]` guards, parallel-subshell harness, detached-HEAD attach ladder, `merge --ff-only` with hooks disabled. Differences are parameterizable (fetch direction, attach priority list, fail policy = die vs WARN). A `_land_sync_sub <src> <dst> <expected_sha> <fetch_ref> <attach_priority...> <on_fail>` helper would halve both blocks (~30 LOC saved). Surfaced by simplify multi-agent review during the §Land-flow step-10 add.
- **Direct integration test for submodule ancestry preflight.** The preflight gates step-12 propagation by checking `main-clone-HEAD` is ancestor of `slot-clone-HEAD` for each gitlink-moved sub. Testing requires source's submodule clone to diverge from its tracked gitlink without tripping `M sub` in status (the main_dirty check). `submodule.sub.ignore=all` silences status but also breaks `git diff --raw` from finding the gitlink change in slot. `ignore=dirty` still shows commit divergence. Probable solution: configure `extensions.worktreeConfig=true` + set ignore per-worktree on the source main worktree only, leaving slot diff intact. Or test the preflight via a unit-style harness instead.

## Surfaced by multi-agent audit (2026-05)

Reviewers ran over the perf changes (`a504730`..`c17a34b`). Real bugs fixed inline (`5af9237`, `<this-commit>`); these are remaining deferred follow-ups.

### Correctness / hardening (deferred)

- **Cross-acquire `<source>/.git/config` lockfile contention (theoretical)** — `submodules::update` phase 1 sequentially writes `submodule.<name>.url` per submodule via `git -C <slot> config …`, which targets the shared `<source>/.git/config`. Two parallel acquires on different slots in the same source can cluster their writes (~17 × ~5ms each per acquire) and contend on git's `O_EXCL` lockfile retry. Not observed in 4-way stress test on macmini (window is small and git's internal retry usually wins). If it surfaces: take a per-source mutex around the phase-1 loop (cheap, scoped narrower than `pool_mu`), or set `extensions.worktreeConfig=true` and write submodule config per-worktree.
### Test coverage gaps (deferred)

- **`--exclude-submodule-tags` deinit path completely untested end-to-end** — `submodules.rs` deinit-on-tag-excluded code path has only unit tests for `parse_gitmodules_*`. Add a fixture with two submodules, one tagged `editor` via `worktreePoolTag = editor`, acquire with `--exclude-submodule-tags editor`, assert the tagged submodule's working dir is absent post-acquire and the other is checked out.
- **Group-less pool full lifecycle untested** — only the unit `classify_groupless` covers it. Add a smoke test that inits a pool with no groups and runs full acquire→release, verifying the canonical name pattern is `slot-N`.

When tackling these, the existing `make_fixture_with_n_submodules(dir, n)` helper in `tests/smoke.rs` covers most of the needed scaffolding.

## Won't-do (decided this session)

- **Migrate read ops to `gix` or `git2`** — Profiled (0981f5c, f22a5cf). Both add 1-2 MB binary; the spawn-savings (~10 ms / acquire) are dwarfed by network-bound submodule clones. Shell-out + `opt-level = "s"` is the local optimum at 669 KB binary.
- **Consumer-supplied submodule URL rewrites via callback** — current pool-config-driven `submodule_mirror_mode + submodule_mirror_base` covers the two consumer patterns (bare-mirror, git-modules); a plugin shape isn't justified by 2 consumers.
- **`rayon` for parallel submodule update / scans** — earlier TODO entries proposed `rayon::par_iter`. We chose `parallel::{for_each, try_for_each}` (in-house wrapper over `std::thread::scope` + `Builder::spawn_scoped` with inline-fallback) for `submodules::update_recursive` and `delete_branch_recursive`: N is small (≤max-slots, ≤submodule-count), thread-spawn cost is negligible compared to git-process spawn, and `rayon` adds ~50KB binary + a transitive scheduler we don't need. The wrapper exists because raw `Scope::spawn` panics on OS thread-create failure under `panic = "abort"` (verified crash class, May 2026). The remaining "parallel ls / parallel same-SHA scan" TODO entries above should reuse `parallel::*` if implemented.
