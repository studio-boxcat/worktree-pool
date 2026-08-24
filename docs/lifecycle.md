# Lifecycle invariants

> **Related:** [[CLAUDE.md]], [[cli.md]], [[wt.md]] (land flow + cleanup classifier)

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
6. Materialize at canonical path: fresh → `git worktree add --detach`; recycled → remove any leftover `<gitdir>/index.lock` (see [[#crash-recovery]]), then `git reset --hard <full_sha>`. **Never `git clean`** — untracked files are caller's warmth.
7. Force-create branch (`update-ref refs/heads/<L> HEAD && symbolic-ref HEAD refs/heads/<L>`). **This flips idle → held.** (Avoids `git checkout -B`'s 600ms of per-file filter-process pings on an already-correct tree.)
8. Drop pool-wide mutex.
9. Submodule update, two-phase: (a) sequential `git config submodule.<name>.url` writes per submodule under a per-source mutex (`<source-gitdir>/worktree-pool-config.lock`) so parallel acquires sharing a source don't fight on `<source>/.git/config`'s lockfile; (b) parallel per-submodule `git submodule update` via `parallel::try_for_each`, then attach each submodule to a branch matching the lease. Each worker recurses into nested `.gitmodules` end-to-end, so the full tree fans out in parallel. Tag exclusion via `--exclude-submodule-tags` (see [[#submodule-filtering-worktreepooltag]]).
10. Fire `wt_post_acquire` if the source ships `.wt-hooks.sh`. Fail-loud — a non-zero hook fails the acquire before any path is printed. Runs for direct `worktree-pool acquire` (build pools) and `wt go`. See [[wt.md#hooks-sourcewt-hookssh]].
11. Drop init mutex; print canonical path on stdout (last line).

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

When all slots in the requested group are held, `acquire` errors with the slot
table inline plus the next command verbatim:

```
acquire failed: all 16 ios slots are held.

Held slots in pool /Users/x/.worktree-pool/myapp:
  ios-0 (lease: abc12345-ios)
  ios-1 (lease: feature-x)
  ...

Release one with: worktree-pool --pool myapp release --lease <L>
```

There is no GC — the operator releases manually based on the table.

---

## Over-provisioned pools

A pool is **over-provisioned** when canonical dirs at N >= `max_slots` exist —
reachable when the operator lowers `max_slots` in `config.yaml` after slots were
materialized. `acquirable_ns` is bounded for *fresh* creation (`0..max_slots`,
so the pool never grows past `max_slots`) but unbounded for *recycled-idle* dirs
at N >= `max_slots`. Surplus N's are preferred on acquire (lowest-N-first),
eating down the over-provision over time.

No operator-facing GC. Manual: `git -C <source> worktree remove --force
<pool>/slot-N`.

---

## Submodule mirror (mandatory when submodules exist)

A submodule's effective fetch URL at acquire time is rewritten to a **local
mirror** by `submodule_mirror_mode`:

| Mode | Effective URL | Resolves local-only pins? |
|------|---------------|---------------------------|
| `source-submodules` | `<base>/.git/modules/<composedName>` | **yes** — reads a working clone's own object store |
| `bare-mirror` | `<base>/<org>/<repo>.git` | only if the mirror is fresh |

Both need `submodule_mirror_base`. When the source declares submodules a mirror
is **mandatory** — there is deliberately no declared-URL fallback. An absent
mirror could only reach the network, failing mid-acquire with a cryptic `not our
ref` the moment a pin is local-only (a freshly-bumped-but-unpushed submodule —
the common dev case). So we fail loud:

- **`init`** refuses a submodule-bearing source with no mirror (no pool created).
- **`acquire`** backstops pools predating the gate (or whose source gained
  submodules since): it bails **before** the idle→held flip, leaving the slot
  detached (idle) and reclaimable rather than HELD with a half-fetched tree.

For a working-clone source you actively commit in, use `source-submodules` with
`base = source`: it resolves whatever the source HEAD references, pushed or not.
`base` may differ from `source` — e.g. a bare source mirrored from its sibling
working clone's `.git/modules`.

## Submodule filtering (`worktreePoolTag`)

Submodule taxonomy lives in the source repo's `.gitmodules` (version-controlled,
propagates on next checkout). The tool reads `worktreePoolTag = <tag>` lines
(case-insensitive key — git lowercases on read).

```ini
[submodule "Packages/com.unity.ide.rider"]
    path = Packages/com.unity.ide.rider
    url = git@github.com:org/com.unity.ide.rider.git
    worktreePoolTag = editor
```

`acquire --exclude-submodule-tags <t1,t2>` deinits + skips submodules whose tag
matches:

```sh
# CI build skips editor-only modules
worktree-pool --pool myapp acquire abc12345 --commit abc12345 --group ios --exclude-submodule-tags editor

# Dev session includes them
worktree-pool --pool myapp acquire feature-x --group ios
```

Tag filter applies at top level only; nested submodules always init when their
parent is included. `validate-gitmodules` warns on misspelled `worktreePool*`
keys (catches the `worktreePoolTags` plural typo etc).
