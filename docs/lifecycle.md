# Lifecycle invariants

> **Related:** [[../CLAUDE.md]], [[cli.md]], [[wt.md]] (land flow + cleanup classifier)

## `acquire`

1. Resolve `--commit` (default `default_commit` from config) against source repo → full SHA.
2. Take pool-wide mutex.
3. If `--unique-sha`, scan held locks for matching `full_sha`; refuse on hit.
4. Check capacity (`count_occupying_in_group >= max_slots` → refuse with the slot table inline). Counts every `Renamed` slot — held-with-lock against its lock's group, zombies (no/bad lock) against every group conservatively. With `smallest_free_n` no longer bounded by `max_slots` (see [[#over-provisioned-pools]]), the prior-step `reclaim_stale` always relocates zombies first, so zombie-counting is defense-in-depth rather than the primary line.
5. Iterate acquirable Ns (fresh + recycled-idle, smallest first). Try per-slot init mutex on each (`O_EXCL`); first success wins. Heartbeat mtime every 30s during init.
6. Materialize: fresh → `git worktree add --detach <pool>/{group}-N <full_sha>`; recycled → `git -C <slot> reset --hard <full_sha>`. **Never `git clean`** — untracked files are caller's warmth.
7. Write lock at `<source>/.git/worktrees/<id>/worktree-pool/lock` (atomic; tempfile + rename) — held marker lands BEFORE the rename.
8. Rename via `fs::rename` + `git worktree repair` (`git worktree move` refuses on slots with submodules — see `git.rs::worktree_rename`). Also rewrites every submodule admin's `core.worktree` (anchored on the pool-key segment, idempotent self-heal).
9. Force-create branch: `git -C <pool>/<name> update-ref refs/heads/<name> HEAD && symbolic-ref HEAD refs/heads/<name>`. (Avoids `git checkout -B`'s 600ms of per-file filter-process pings on a tree that's already at the right state.)
10. Drop pool-wide mutex (slot is now visibly held under user-name; submodule init below is per-slot work guarded by the still-held init mutex).
11. Submodule update, two-phase to dodge `<source>/.git/config` lockfile contention: (a) sequential `git config submodule.<name>.url` writes per submodule, applying URL overrides per pool config; (b) parallel per-submodule `git submodule update <path>` via `parallel::try_for_each` (inline-fallback on OS thread-create failure — see `parallel.rs`), then in the same thread `update-ref refs/heads/<name> HEAD && symbolic-ref HEAD refs/heads/<name>` to attach the submodule to a branch matching the parent slot's name (gives commits a push-ready label and a stable ref for `wt land` to fetch by — see [[wt.md#land-flow]]). Recursive into nested submodules with the same branch name. Tag exclusion via `--exclude-submodule-tags` against `worktreePoolTag` in `.gitmodules`.
12. Release init mutex; print path on stdout (last line).

## `release`

1. Take pool-wide mutex.
2. Run `slot::reclaim_stale` to fix any leftover state from a prior crash (see [[#crash-recovery]]).
3. Short-circuit: if the slot dir doesn't exist (already released, or just reclaimed), exit 0.
4. Read lock to recover `group`.
5. Detach HEAD; `branch -D <name>` (local); `push --delete origin <name>` (best-effort, no-op if `origin` is a bare mirror). Recursively mirror this in every submodule (`detach + branch -D <name>` per submodule, parallel via `parallel::for_each`) to clean up the per-slot branch step 11 of acquire created.
6. Find smallest free `{group}-N` (unbounded search — see [[#over-provisioned-pools]]; release lands at surplus N >= `max_slots` if every N in `0..max_slots` is occupied).
7. Un-rename via `fs::rename` + `git worktree repair` + submodule `core.worktree` self-heal (same primitives as acquire's rename).
8. **Delete lock LAST.** This is the only step that transitions the slot from held → idle on disk; all earlier steps preserve the lock as the authoritative "still owned" signal so a crash mid-flight is recoverable by replay.
9. Drop mutex.

## Crash recovery

The on-disk encoding is built so any crash leaves a state that the next `acquire` or `release` can finish. Both operations call `slot::reclaim_stale` immediately after taking the pool mutex, before reading any other state.

**Invariants by source.**

- **acquire** writes the lock BEFORE the rename. A crash before the rename leaves the slot at canonical `{group}-N` with lock + detached HEAD; a crash after the rename but before `checkout_force_branch` leaves it at user-name with lock + detached HEAD. Both are reachable by reclaim (canonical case via the HEAD-detached disambiguator; user-name case via re-running `release --name <user-name>`).
- **release** removes the lock AFTER the un-rename. A crash before the rename leaves the slot at user-name with lock present (every step replayable); a crash between rename and lock removal leaves an orphan lock at canonical (reclaim disambiguates by HEAD state and removes it).

**Pool-ownership marker.** A sticky file at `<source>/.git/worktrees/<id>/worktree-pool/created` is co-written with the lock at `acquire` time and **never removed** (release/reclaim/cleanup all leave it). Its presence is the positive signal "this gitdir was once pool-acquired." Without it, `reclaim_stale` would silently absorb any `(Renamed dir, no lock)` it sees — including manual `git worktree add`s and operator-scratch dirs that happen to share a name. The marker turns that case into a loud refusal. See `fs_paths::slot_created_marker` and `release::reclaim_stale`.

**`reclaim_stale` rules.**

| dir name | lock | marker | HEAD | meaning | action |
|---|---|---|---|---|---|
| renamed | present | any | any | held (or release crashed mid-slow-ops) | leave; replay if user re-runs `release` |
| renamed | absent | **present** | any | **legacy zombie** (pre-fix release crash) | replay release tail: detach, delete branch, `worktree_rename` to canonical |
| renamed | absent | **absent** | any | **unmanaged dir** (manual `git worktree add`, hand-deleted lock+marker) | leave untouched, log warn — operator clears via `wt rm --force` |
| canonical | present | any | on branch | live held (or acquire crashed late) | leave |
| canonical | present | any | detached | **post-rename orphan lock** (release crashed late) | remove lock |
| canonical | absent | any | any | idle | leave |

The HEAD-detached check is the disambiguator for the canonical-with-lock row: a live held slot is on its branch (`acquire::checkout_force_branch`); a canonical-named dir with detached HEAD never reached that step (or reached it then was undone by release). See `slot::is_post_release_orphan`.

**Stale `git index.lock` sweep.** Orthogonal to the table above and run once per enumerated slot: `<source>/.git/worktrees/<id>/index.lock` is removed iff **0 bytes AND mtime older than 60s**. The lock is git's, not ours — it leaks when a git process dies between `open(O_CREAT|O_EXCL)` and the first write (SIGKILL from harness timeouts, panic, untracked-cache writeback aborting under concurrent-git contention). The 0-byte + age guard protects live `git status` / `git commit` from inside a held slot (they hold a non-empty, young lock for milliseconds). Non-zero locks are left alone — they may be partial writes the operator wants to inspect. See `release::sweep_stale_index_lock`.

**Migration from pre-marker pools.** Pools created before the provenance marker have lock files but no marker. At the start of every `reclaim_stale`, an idempotent auto-stamp pass writes the marker on any slot whose lock is present — lock presence proves pool ownership (only `acquire` writes locks). Pre-fix `(Renamed, no lock)` zombies have already lost provenance and fall into the **unmanaged dir** row above; clear them with `wt rm --force <name>` once.

**Residual modes still needing operator action:**

- **Ghost dir** (`.git` gitlink missing or dangling — typically from a half-completed `worktree remove` whose working-tree rm couldn't finish, e.g. an IDE holding a file open): no git state to reach the slot through. `wt go/cleanup/rm` all detect this and refuse with `🔴 BROKEN` (see [[wt.md#cleanup-classifier]]). Recover with `rm -rf <slot-path>`.
- **Unmanaged dir** (the row above): pool refuses to touch it, but the dir continues to occupy a name slot. `wt rm --force <name>` releases through the normal path; alternatively `git worktree remove <slot>` then `rm -rf <slot>` for non-pool worktrees.

## Mutex liveness

**Pool-wide mutex** (`<pool>/.meta/pool.lock`): the holder writes its PID into the file at create time. On contention, a new acquire reads the PID and probes `kill(pid, 0)` — if the holder process is gone (cmd+W → SIGHUP, SIGKILL, OOM, panic under `panic=abort`), the lock is reclaimed immediately. Mtime ≥ `POOL_MUTEX_STALE_AFTER` (120s) is kept as a fallback for legacy lock files and the microsecond create-then-write-PID race window.

**Init mutex** (`<pool>/.meta/init/<slot-id>.lock`): mtime-heartbeated every 30s during init; reclaimable by another acquire after 60min of no heartbeat (covers SIGKILL'd cold submodule clones), with a stderr warning logged. Manual cleanup via `worktree-pool --pool <key> unstick [--slot <id>]`.

## Capacity-bound failures

When all slots are held, `acquire` errors with the slot table inline plus the next command verbatim:

```
acquire failed: all 16 ios slots in use.

ID     STATE  NAME              GROUP  AGE
ios-0  held   abc12345          ios    2h
ios-1  held   feature-x         ios    3d
...

Release one with: worktree-pool --pool myapp release --name <n>
```

There is no GC. The operator releases manually based on the table.

---

## Over-provisioned pools

A pool is **over-provisioned** when total dirs in `<pool>/` exceeds `max_slots`. Reachable through two paths:

- **Zombie storm**: a SIGINT mid-release leaves a `Renamed`-no-lock zombie. Reclaim relocates it to a canonical-N, growing idle-canonical count. With enough such zombies, `0..max_slots` fills with idle canonicals while at least one named slot is still held — total dirs = max_slots + held_count.
- **Reduced `max_slots`**: operator edits `<pool>/.meta/config.yaml` to lower `max_slots` after slots were materialized.

`smallest_free_n` is unbounded — release lands at the smallest free N regardless of `max_slots`, including N >= max_slots (surplus). `acquirable_ns` is bounded for *fresh* dirs (`0..max_slots`, so the pool never grows past max_slots via fresh creation) but unbounded for *recycled-idle* dirs (surplus N's get reused on acquire, eating down the over-provision over time). Together: release never wedges, and the pool self-heals via subsequent acquire activity.

There is currently no operator-facing GC for surplus dirs that the operator wants permanently gone (e.g. after reducing `max_slots`). Manual: `git -C <source> worktree remove --force <pool>/slot-N` for the surplus N's.

---

## Same-SHA exclusion

Opt-in via `--unique-sha` on acquire. Caller asserts "I'm doing duplicate-detectable work; refuse if another slot is already on this SHA." Build callers (CI) opt in; dev callers don't (devs branching off `main` don't want to fight a CI build at the same SHA).

When triggered, the error names the slot, lock holder name, and held-since age. Operator decides whether to wait, reuse the existing slot's output, or release the conflicting slot. The check holds the pool-wide mutex, so it's atomic w.r.t. other acquires. Cross-pool exclusion is not a thing — each pool is its own bucket.

---

## Submodule filtering (`worktreePoolTag`)

Submodule taxonomy lives in source repo's `.gitmodules` (version-controlled, propagates on next checkout). The tool reads `worktreePoolTag = <tag>` lines (case-insensitive on key — git lowercases on read).

```ini
[submodule "Packages/com.unity.ide.rider"]
    path = Packages/com.unity.ide.rider
    url = git@github.com:org/com.unity.ide.rider.git
    worktreePoolTag = editor
```

`acquire --exclude-submodule-tags <t1,t2>` deinits + skips submodules whose tag matches:

```sh
# CI build skips editor-only modules
worktree-pool --pool myapp acquire --name abc12345 --commit abc12345 --group ios --exclude-submodule-tags editor

# Dev session includes them
worktree-pool --pool myapp acquire --name feature-x --group ios
```

Tag filter applies at top level only; nested submodules always init when their parent is included. `worktree-pool --pool <key> validate-gitmodules` warns on misspelled `worktreePool*` keys (catches `worktreePoolTags` plural typo etc).

---

## Why not `git worktree move`?

`git worktree move` refuses to move a worktree that has initialized submodules:

```
fatal: working trees containing submodules cannot be moved or removed
```

Every recycled slot in the pool has submodules — Unity packages, FacebookSDK, etc. — so `git worktree move` is unusable for our rename step. We replace it with three primitives that achieve the same end state:

1. **`std::fs::rename(from, to)`** — atomic on the same filesystem, indifferent to submodules. Moves only the slot's working tree directory.
2. **`git -C <source> worktree repair <to>`** — rewrites `<source>/.git/worktrees/<id>/gitdir` to point at the new path. Idempotent.
3. **Per-submodule `core.worktree` rewrite** — `git worktree repair` does NOT recurse into submodules. We walk `<source>/.git/worktrees/<id>/modules/**/config` ourselves and rewrite each submodule's `core.worktree` value, anchored on the pool-key path segment that precedes the slot name (so a slot named `Packages` doesn't accidentally collide with sub-paths like `Packages/com.foo`). The rewrite is idempotent and self-healing — a stale segment from a partial prior rewrite gets normalized on the next rename. See `git.rs::worktree_rename` and tests `rewrite_slot_segment_*`.
