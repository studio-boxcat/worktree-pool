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

- **`worktree-pool doctor` could check pool initialization** — currently host-level checks only (arch, git, ~/.worktree-pool dir presence, quarantine xattr). Could enumerate registered pools (subdirs of `~/.worktree-pool/` with a `.meta/config.yaml`) and validate each (config schema, source path exists, etc.).
- **Bench fixture is too small** — `scripts/bench-fixture.sh` makes a bare repo with one commit, no submodules. Real cold-acquire cost (the dominant wall time) isn't measured. A heavier fixture with a few mock submodules would expose the parallel-submodule-recursion win when implemented.

## Won't-do (decided this session)

- **Migrate read ops to `gix` or `git2`** — Profiled (0981f5c, f22a5cf). Both add 1-2 MB binary; the spawn-savings (~10 ms / acquire) are dwarfed by network-bound submodule clones. Shell-out + `opt-level = "s"` is the local optimum at 669 KB binary.
- **Consumer-supplied submodule URL rewrites via callback** — current pool-config-driven `submodule_mirror_mode + submodule_mirror_base` covers the two consumer patterns (bare-mirror, git-modules); a plugin shape isn't justified by 2 consumers.
