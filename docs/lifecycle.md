# Lifecycle invariants

> **Related:** [[CLAUDE.md]], [[cli.md]], [[submodules.md]] (mirror + tag filtering), [[wt.md]] (land flow + cleanup classifier)

## Identity model

Two identifiers, and conflating them is the classic bug:

| | Assigned by | Form | Means |
|---|---|---|---|
| **slot id** | the pool | `{group}-{N}`, or `slot-{N}` ungrouped | *where* — the directory |
| **lease** | the caller (`--lease`) | anything | *what for* — the claim on it |

Slot dirs live at their canonical path forever; there is no rename. Stable
absolute paths keep abs-path-keyed caches warm across acquire cycles (Unity Bee
compile cache, watchman watches, IDE indexes), at the cost of a small git probe
per lease → slot lookup.

The lease is stored as the slot's branch ref, so `git symbolic-ref --short HEAD`
is the source of truth for who holds what, and a detached HEAD *is* idle.
`release`/`inspect`/`path` also accept a slot id, so an operator can address a
slot straight off `ls`.

A lease identifies **exactly one** held slot: `acquire` refuses one already held
(exit 6). Nothing downstream would catch a duplicate — step 7 below bypasses
git's same-branch-in-two-worktrees guard, `refs/heads/` is shared across linked
worktrees, and lookup resolves in `read_dir` order, so `release` could silently
detach the wrong slot while a consumer was still using it.

That refusal is also the **only** duplicate-work guard. The pool deliberately
has no commit-keyed exclusion: two leases at one SHA are two jobs, which is the
normal case (a build's player and bundles halves; both platforms off a release
commit). A caller that wants "don't run this twice" derives the lease from what
it produces, and gets the refusal for free.

## `acquire --lease <L>`

1. Resolve `--commit` (default `default_commit`) against source → full SHA.
2. Take pool-wide mutex (flock).
3. Refuse if the lease is already held (see [[#identity-model]]).
4. Check capacity (`count_held_in_group >= max_slots` → refuse with the slot table inline).
5. Iterate acquirable Ns (canonical `0..max_slots` with detached HEAD, plus surplus recycled-idle N >= max_slots — see [[#over-provisioned-pools]]). Try each slot's init mutex (flock); first success wins.
6. Materialize at canonical path: fresh → `git worktree add --detach`; recycled → remove any leftover `<gitdir>/index.lock` (see [[#crash-recovery]]), `git reset --hard <full_sha>`, then sweep stranded git working dirs (see [[#the-stranded-working-dir-sweep]]).
7. Force-create branch (`update-ref refs/heads/<L> HEAD && symbolic-ref HEAD refs/heads/<L>`). **This flips idle → held.** (Avoids `git checkout -B`'s 600ms of per-file filter-process pings on an already-correct tree.)
8. Drop pool-wide mutex.
9. Submodule update: sequential URL rewrites, then a parallel per-submodule `update` that recurses into nested `.gitmodules` end-to-end, attaches each to a lease-named branch, and re-runs step 6's sweep one level down. See [[submodules.md]].
10. Fire `wt_post_acquire` if the source ships `.wt-hooks.sh`. Fail-loud — a non-zero hook fails the acquire before any path is printed. Runs for direct `worktree-pool acquire` (build pools) and `wt go`. See [[wt.md#hooks-sourcewt-hookssh]].
11. Drop init mutex; print the canonical path (see [[cli.md#output-contract]]).

### The stranded working-dir sweep

Dropping a submodule from `.gitmodules` leaves its working dir behind: checkout
removes the gitlink but not the directory, and `reset --hard` never touches
untracked paths. The recycle sweep removes untracked dirs that contain a `.git`
entry.

This is the **one** exception to never running `git clean` — untracked files are
the caller's warmth, the whole reason slots are recycled rather than recreated.
An undeclared repo copy earns the exception because it sits as duplicate content
beside the real one and checkout consumers (Unity import) can't tell them apart.

A *nested* submodule dropped from an outer submodule's `.gitmodules` strands
inside that outer tree, out of the parent sweep's sight — hence the second pass
in step 9.

## `release --lease <L>`

1. Take pool-wide mutex (flock).
2. `slot::find_by_lease` — scan held slots, matching the lease against the branch ref. Not found → idempotent success (already released, never acquired, or branch hand-deleted).
3. **`detach_head`** — flips held → idle (under pool mutex, no race). Then `branch -D <L>` (local), `push --delete origin <L>` (best-effort; no-op against a bare mirror). Mirror recursively in every submodule.
4. Drop pool mutex.

Release touches only refs (`detach_head` is `rev-parse` + `update-ref
--no-deref`), never the index — so a leftover `index.lock` can't block it and
there's no sweep here. The slot dir stays canonical, ready for the next acquire
to land with caches still warm.

## Crash recovery

Release is idempotent — replaying after a crash converges. There is **no**
auto-replay of crashed acquire/release and no periodic recovery sweep.

**Why no auto-replay?** The init-mutex flock auto-releases on *any* exit, so
without a separate in-flight marker (which itself can leak) there's no safe way
to tell "completed and exited" from "crashed mid-flight". Heuristic auto-replay
would either false-positive on healthy held slots or need that leaky marker. We
chose explicit operator recovery.

**Operator recovery paths:**

- **Crash mid-acquire before branch creation.** Slot has detached HEAD = idle.
  No recovery — next acquire reclaims it as recycled-idle.
- **Crash mid-acquire during submodule init.** Slot is held with partial
  submodule state. `release --lease <L>` finds it and runs the idempotent
  detach + branch deletion. One command.
- **Crash mid-release after detach.** Slot is idle; branch ref may be orphaned.
  Re-running `release` returns "already released". The orphan ref is
  harmless (`git gc` cleans it; `git branch -D <name>` also works).

**Leftover `git index.lock`.** Git's, not ours — it leaks when a git process
dies between `open(O_CREAT|O_EXCL)` and the first write (crashed lazygit/`git
status`, SIGKILL, panic, untracked-cache writeback aborting under contention).
It only matters on the recycle path: a recycled slot's `git reset --hard` would
fail `EEXIST` on it. So `acquire` removes
`<source>/.git/worktrees/<id>/index.lock` unconditionally right before the
recycled `reset --hard` (step 6). Race-free — the slot is idle and acquire holds
the pool + slot init mutex, so no legitimate git process owns the lock — and it
catches partial locks a staleness heuristic would skip. Held slots are never
touched: their `index.lock` belongs to a live session.

**Residual mode still needing operator action:**

- **Ghost dir** (`.git` gitlink missing or dangling — typically a half-completed
  `worktree remove` whose working-tree rm couldn't finish, e.g. an IDE holding a
  file open): no git state to reach the slot through. `wt go/cleanup/release` all
  refuse with `🔴 BROKEN` (see [[wt.md#cleanup-classifier]]). Recover with
  `rm -rf <slot-path>`.

## Capacity-bound failures

When every slot in the requested group is held, `acquire` exits 4 and lists the
held slots with their leases on stderr, plus the `release` command to run. There
is no GC — the operator picks from that list.

## Over-provisioned pools

A pool is **over-provisioned** when canonical dirs at N >= `max_slots` exist —
reachable when the operator lowers `max_slots` in `config.yaml` after slots were
materialized. `acquirable_ns` is bounded for *fresh* creation (`0..max_slots`,
so the pool never grows past `max_slots`) but unbounded for *recycled-idle* dirs
at N >= `max_slots`. Surplus N's are preferred on acquire (lowest-N-first),
eating down the over-provision over time.

No operator-facing GC. Manual: `git -C <source> worktree remove --force
<pool>/slot-N`.
