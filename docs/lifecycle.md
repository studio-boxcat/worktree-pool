# Lifecycle invariants

> **Related:** [[../CLAUDE.md]], [[cli.md]], [[wt.md]] (land flow + cleanup classifier)

## Identity model

Slot dirs live at canonical paths forever — `{group}-{N}` for grouped pools,
`slot-{N}` otherwise. There is no rename. The user-given `NAME` from
`acquire NAME` is **the git branch name** inside the slot; `git symbolic-ref
--short HEAD` is the source of truth for which slot belongs to which user
context. Lookup by name (`release NAME`, `inspect NAME`, `path NAME`) scans
held slots and matches on branch.

This keeps absolute paths stable across acquire cycles, so abs-path-keyed
caches (Unity Bee compile cache, watchman watches, IDE absolute-path indexes)
stay warm. The trade-off is a small per-call git probe for name → slot
resolution; cheap vs. the cache rebuild cost.

## `acquire NAME`

1. Resolve `--commit` (default `default_commit` from config) against source repo → full SHA.
2. Take pool-wide mutex (flock).
3. Run `reclaim_stale` to fix any leftover state from a prior crash (see [[#crash-recovery]]).
4. If `--unique-sha`, scan held locks for matching `full_sha`; refuse on hit, reporting the holder's branch name.
5. Check capacity (`count_held_in_group >= max_slots` → refuse with the slot table inline).
6. Iterate acquirable Ns (canonical Ns 0..max_slots without a held lock; plus surplus N >= max_slots that exist as recycled-idle — see [[#over-provisioned-pools]]). Try per-slot init mutex (flock) on each; first success wins.
7. Materialize at canonical path: fresh → `git worktree add --detach <pool>/{group}-N <full_sha>`; recycled → `git -C <slot> reset --hard <full_sha>`. **Never `git clean`** — untracked files are caller's warmth.
8. Write lock at `<source>/.git/worktrees/<id>/worktree-pool/lock` (atomic; tempfile + rename). Lock body: `started_at`, `full_sha`, optional `group`.
9. Force-create branch: `git -C <slot> update-ref refs/heads/NAME HEAD && symbolic-ref HEAD refs/heads/NAME`. (Avoids `git checkout -B`'s 600ms of per-file filter-process pings on a tree that's already at the right state.)
10. Drop pool-wide mutex.
11. Submodule update, two-phase: (a) sequential `git config submodule.<name>.url` writes per submodule, applying URL overrides per pool config; (b) parallel per-submodule `git submodule update <path>` via `parallel::try_for_each` (inline-fallback on OS thread-create failure), then `update-ref refs/heads/NAME HEAD && symbolic-ref HEAD refs/heads/NAME` to attach each submodule to a branch matching the parent slot's name (push-ready label and stable ref for [[wt.md#land-flow]] to fetch by). Recurses into nested submodules. Tag exclusion via `--exclude-submodule-tags` against `worktreePoolTag` in `.gitmodules`.
12. Drop init mutex (flock released on file close); print canonical path on stdout (last line).

## `release NAME`

1. Take pool-wide mutex (flock).
2. Run `reclaim_stale`.
3. `slot::find_by_name(NAME)` — scan held slots, match by `git symbolic-ref --short HEAD`. Not found → idempotent success (already released, never acquired, or branch was hand-deleted).
4. `detach_head`, `branch -D NAME` (local), `push --delete origin NAME` (best-effort; no-op if `origin` is a bare mirror). Recursively mirror in every submodule.
5. **Delete lock LAST.** This is the only step that transitions held → idle on disk; all earlier steps preserve the lock as the authoritative "still owned" signal so a crash mid-flight is recoverable by replay.
6. Drop pool mutex.

Slot dir stays at its canonical path — ready for the next acquire to land in it with new caches still warm.

## Crash recovery

The on-disk encoding is built so release is idempotent — replaying it after a
crash converges. `reclaim_stale` runs immediately after the pool mutex in
both `acquire` and `release`, but its scope is intentionally narrow: it only
sweeps foreign git artifacts (`index.lock`). It does **not** auto-replay
crashed acquire/release.

**Why no auto-replay?** Without a per-slot in-flight signal that survives
normal process exit, there's no way to safely tell "completed and exited"
from "crashed mid-flight" — the init-mutex flock is auto-released by the OS
on *any* exit, normal or otherwise. Heuristic auto-replay would either
false-positive (cleaning up healthy held slots) or require a separate
in-flight marker that itself can leak. We chose explicit operator recovery
instead.

**Operator recovery paths:**

- **Crash mid-acquire after lock write, before branch checkout.** Slot has
  lock + detached HEAD + no branch named `NAME`. `release NAME` (by branch)
  returns "already released" — there's no matching branch. The operator
  releases via the canonical-id fallback: `worktree-pool release <slot-id>`
  (e.g. `release ios-0`). The slot's gitdir resolves, lock is found, and
  `release_tail` cleans it up.

- **Crash mid-acquire during submodule init.** Slot has lock + branch
  attached + partial submodule state. `release NAME` finds the slot by
  branch and runs `release_tail` — branch deletion + lock removal are
  idempotent. Recovery is a single command.

- **Crash mid-release.** Lock removal is the LAST step; every earlier step
  is idempotent. Re-running `release NAME` (or `release <slot-id>` if the
  branch is already gone) converges.

**Stale `git index.lock` sweep.** What `reclaim_stale` actually does. Run
once per enumerated slot: `<source>/.git/worktrees/<id>/index.lock` is
removed iff **0 bytes AND mtime older than 60s**. The lock is git's, not
ours — it leaks when a git process dies between `open(O_CREAT|O_EXCL)` and
the first write (SIGKILL from harness timeouts, panic, untracked-cache
writeback aborting under concurrent-git contention). The 0-byte + age guard
protects live `git status` / `git commit` from inside a held slot (they hold
a non-empty, young lock for milliseconds). Non-zero locks are left alone —
they may be partial writes the operator wants to inspect.

**Residual mode still needing operator action:**

- **Ghost dir** (`.git` gitlink missing or dangling — typically from a
  half-completed `worktree remove` whose working-tree rm couldn't finish,
  e.g. an IDE holding a file open): no git state to reach the slot through.
  `wt go/cleanup/release` all detect this and refuse with `🔴 BROKEN`
  (see [[wt.md#cleanup-classifier]]). Recover with `rm -rf <slot-path>`.

## Capacity-bound failures

When all slots in the requested group are held, `acquire` errors with the
slot table inline plus the next command verbatim:

```
acquire failed: all 16 ios slots are held.

Held slots in pool /Users/x/.worktree-pool/myapp:
  ios-0 (branch: abc12345)
  ios-1 (branch: feature-x)
  ...

Release one with: worktree-pool --pool myapp release <branch-name>
```

There is no GC. The operator releases manually based on the table.

---

## Over-provisioned pools

A pool is **over-provisioned** when canonical dirs at N >= `max_slots` exist
— reachable when the operator edits `<pool>/.meta/config.yaml` to lower
`max_slots` after slots were materialized. `acquirable_ns` is bounded for
*fresh* creation (`0..max_slots`, so the pool never grows past `max_slots`),
but unbounded for *recycled-idle* dirs at N >= `max_slots`. Surplus N's get
preferred on acquire (lowest-N-first), eating down the over-provision over
time.

No operator-facing GC. Manual: `git -C <source> worktree remove --force
<pool>/slot-N` for the surplus N's.

---

## Same-SHA exclusion

Opt-in via `--unique-sha` on acquire. Caller asserts "I'm doing
duplicate-detectable work; refuse if another slot is already on this SHA."
Build callers (CI) opt in; dev callers don't (devs branching off `main`
don't want to fight a CI build at the same SHA).

When triggered, the error names the slot id, branch name, and held-since
age. Operator decides whether to wait, reuse the existing slot's output, or
release the conflicting slot. The check holds the pool-wide mutex, so it's
atomic w.r.t. other acquires. Cross-pool exclusion is not a thing — each
pool is its own bucket.

---

## Submodule filtering (`worktreePoolTag`)

Submodule taxonomy lives in source repo's `.gitmodules` (version-controlled,
propagates on next checkout). The tool reads `worktreePoolTag = <tag>` lines
(case-insensitive on key — git lowercases on read).

```ini
[submodule "Packages/com.unity.ide.rider"]
    path = Packages/com.unity.ide.rider
    url = git@github.com:org/com.unity.ide.rider.git
    worktreePoolTag = editor
```

`acquire --exclude-submodule-tags <t1,t2>` deinits + skips submodules whose
tag matches:

```sh
# CI build skips editor-only modules
worktree-pool --pool myapp acquire abc12345 --commit abc12345 --group ios --exclude-submodule-tags editor

# Dev session includes them
worktree-pool --pool myapp acquire feature-x --group ios
```

Tag filter applies at top level only; nested submodules always init when
their parent is included. `worktree-pool --pool <key> validate-gitmodules`
warns on misspelled `worktreePool*` keys (catches `worktreePoolTags` plural
typo etc).
