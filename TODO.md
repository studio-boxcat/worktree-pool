# TODO

Deferred work for worktree-pool. See `CLAUDE.md` for the design contract.

## Efficiency

Surfaced by efficiency-review agents during the v0.1.1 profile pass. Rust paths are sub-µs (yaml parse 269 ns); the cost is dominated by git subprocess spawns. The ones below are the biggest unrealized wins.

- **Parallel submodule recursion** (`src/submodules.rs:update_recursive`) — top-level submodules already batched into one `git submodule update --init <p1> <p2>...`, but recursion into each child runs sequentially (`for e in &included { update_recursive(...) }`). Cold-clone case: nested submodules clone serially. Spawn-per-child via `rayon::par_iter` (mind git's lockfile per-repo — different child repos are independent). For Unity-Packages with K nested `.gitmodules`, up to Kx on cold-clone acquire (the multi-minute path that dominates real-world use). **Highest-impact win.**
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

## Test scaffolding

- **`acquire_dev` doesn't accept `GIT_ALLOW_PROTOCOL=file`** — every submodule-using test (~6 sites: smoke.rs:666, 714, 754 + the two new land tests) bypasses it with an open-coded `Command::cargo_bin(...).args(...).env("GIT_ALLOW_PROTOCOL", "file").output()`. Add an `acquire_dev_sub(key, name)` variant or thread an optional env via builder; cuts ~6 lines per call site.
- **Repeated `git config user.email/user.name`** — every test that commits against a fresh fixture sets these by hand (4× in this commit alone). A `git_commit(dir, msg)` helper that sets `GIT_AUTHOR_*` / `GIT_COMMITTER_*` env once would deduplicate widely.

## Surfaced by multi-agent audit (2026-05)

Reviewers ran over the perf changes (`a504730`..`c17a34b`). Real bugs fixed inline (`5af9237`, `<this-commit>`); these are remaining deferred follow-ups.

### Correctness / hardening (deferred)

- **Cross-acquire `<source>/.git/config` lockfile contention (theoretical)** — `submodules::update` phase 1 sequentially writes `submodule.<name>.url` per submodule via `git -C <slot> config …`, which targets the shared `<source>/.git/config`. Two parallel acquires on different slots in the same source can cluster their writes (~17 × ~5ms each per acquire) and contend on git's `O_EXCL` lockfile retry. Not observed in 4-way stress test on macmini (window is small and git's internal retry usually wins). If it surfaces: take a per-source mutex around the phase-1 loop (cheap, scoped narrower than `pool_mu`), or set `extensions.worktreeConfig=true` and write submodule config per-worktree (requires checking git's worktree-allowed-keys for `submodule.*`).
- **PID-liveness for the per-slot init mutex** — pool-wide mutex now PID-tagged (sub-second hold; immediate reclaim when holder dies — handles cmd+W → SIGHUP, SIGKILL, panic=abort). Init mutex still relies on the 30s mtime heartbeat with 60min stale threshold, so a holder that dies mid-cold-clone wedges that one slot for up to an hour. Same primitive (`read_pid_file` + `pid_alive` in `mutex.rs`) drops in; only complication is that init mutex hold spans submodule clone (potentially long), so PID check has to coexist with the heartbeat — easy: PID dead → reclaim, PID alive → trust the heartbeat as today.
- **`session go` resume should also verify lock presence, not just gitlink validity** — `cmd_go`/`cmd_cleanup`/`cmd_rm` (`bin/worktree-pool-session`) now refuse dirs whose `.git` is missing or dangling — the 🔴 BROKEN ghost-dir case (commit `bbe5937`). Residual gap: the documented "rename succeeded but lock-write failed" crash-recovery state (CLAUDE.md §Crash recovery) leaves a *valid* gitlink but no lock — `cmd_go` still picks that up as resumable, and the cleanup trap then runs a `release` whose lock-read fails and emits cosmetic noise. Harden by requiring lock presence (e.g. `worktree-pool --pool <key> inspect --name <name>` exits 0 only when locked) before treating gitlink-valid as resumable; surface a distinct refusal with the manual-clear hint (`git -C <source> worktree remove --force <slot>`). Low priority — symptom is noise, not data loss; the high-impact ghost-dir half is now handled.

### Test coverage gaps (flagged by 2026-05 audit rounds, not yet addressed)

These were enumerated by audit rounds 3 and 4 (see commit messages on `5af9237`..`dbfd3c6`). All represent real production code paths that ship with no integration test backing them.

- **`acquire_release_with_submodule_rewires_pointers` assertions are weak** — `tests/smoke.rs` runs `git status --porcelain` after acquire and only asserts exit-code success. A bug that silently corrupts URL rewrites would still leave `git status` returning 0. Strengthen by reading `<source>/.git/worktrees/<id>/modules/sub/config` directly and asserting the `[remote "origin"] url` line matches the rewritten path (per pool config's `submodule_mirror_*`).
- **Self-heal test only covers homogeneous stale state** — `release_self_heals_stale_submodule_core_worktree` uses `make_fixture_with_submodule` (1 submodule) and plants a single stale segment. Real partial-failure leaves *heterogeneous* state: SubA at user-name (rewritten), SubB at `stale-name-1`, SubC at `stale-name-2`. Use `make_fixture_with_n_submodules(N=3)`, plant different stale values, verify all converge to canonical after release.
- **`--exclude-submodule-tags` deinit path completely untested end-to-end** — `submodules.rs` deinit-on-tag-excluded code path (around line 263) has only unit tests for `parse_gitmodules_*`. Add a fixture with two submodules, one tagged `editor` via `worktreePoolTag = editor`, acquire with `--exclude-submodule-tags editor`, assert the tagged submodule's working dir is absent post-acquire and the other is checked out.
- **Group-less pool full lifecycle untested** — only the unit `classify_groupless` covers it. Add a smoke test that inits a pool with `--groups ""` and runs full acquire→release, verifying the canonical name pattern is `slot-N` (not `<group>-N`).
- **Symlinked pool root never tested** — `git.rs::worktree_rename` documents (lines 172-180) the constraint that symlink basename must match target dir name; this is currently a doc-only contract. Add a test that creates `~/wtp-test/<key>` as a symlink to `<tmpdir>/<key>` (matching basenames) and runs full lifecycle.
- **`rewrite_config_worktree` walker not unit-tested** — the multi-line / indentation / trailing-newline-preservation / multiple-`worktree =`-lines behavior in `git.rs:217-244` only exercised through `worktree_rename` integration. Add direct unit tests with synthetic config files.

When tackling these, the existing `make_fixture_with_n_submodules(dir, n)` helper in `tests/smoke.rs` covers most of the needed scaffolding.

## Won't-do (decided this session)

- **Migrate read ops to `gix` or `git2`** — Profiled (0981f5c, f22a5cf). Both add 1-2 MB binary; the spawn-savings (~10 ms / acquire) are dwarfed by network-bound submodule clones. Shell-out + `opt-level = "s"` is the local optimum at 669 KB binary.
- **Consumer-supplied submodule URL rewrites via callback** — current pool-config-driven `submodule_mirror_mode + submodule_mirror_base` covers the two consumer patterns (bare-mirror, git-modules); a plugin shape isn't justified by 2 consumers.
- **`rayon` for parallel submodule update / scans** — earlier TODO entries proposed `rayon::par_iter`. We chose `std::thread::scope` instead for the parallel phase 2 of `submodules::update`: N is small (≤max-slots, ≤submodule-count), thread-spawn cost is negligible compared to git-process spawn, and `rayon` adds ~50KB binary + a transitive scheduler we don't need. The remaining "parallel ls / parallel same-SHA scan" TODO entries above should also use `std::thread::scope` if implemented.
