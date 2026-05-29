# Land × submodules

> **Related:** [[wt.md#land-flow]] (the land step contract), [[lifecycle.md#submodule-filtering-worktreepooltag]] (acquire-time clone + URL rewrite)

The rationale behind `wt land`'s submodule handling. [[wt.md#land-flow]] lists
*what* each land step does; this doc explains *why* the submodule steps are
shaped the way they are. Land is **local-only** — every fetch source is another
local clone (`<slot>/<sub>` or `<main>/<sub>`), never `origin`.

Each pool slot's submodule was branched as `<slot-name>` at acquire
([[lifecycle.md#submodule-filtering-worktreepooltag]]); land fetches by that
branch and fast-forwards. All phases are **top-level only** — nested-submodule
propagation is a v2 (recover manually with `git submodule update --init
--recursive`).

## The phases

Land touches submodule clones in four places. Each is idempotent so a re-run
after a fix retries cleanly.

- **Pre-stage sync** (before auto-commit). Stock `git merge` (without
  `--recurse-submodules`, the IDE/`git pull` default) advances the recorded
  gitlink but leaves the submodule working dir at the pre-merge SHA. A blind
  `git add -u` would then stage that stale HEAD as the gitlink, *silently
  regressing the merge*. So when a submodule's working HEAD is an ancestor of
  its index gitlink, run `submodule update` to bring the working dir forward;
  the reverse (working ahead of index) is a real forward bump left for the
  commit; no shared ancestor refuses as divergence.

- **Ancestry preflight** (`_land_preflight_submodules`, before parent advance).
  For each moved gitlink, fetch the slot's branch into main's clone and check
  the slot tip is reachable from main's clone HEAD — surfacing the propagation
  ff's refusal *early*, before the parent advances, with a precise per-sub
  recovery hint (discard main-side, or merge main-side then re-bump). The oracle
  is clone-tip ancestry, not gitlink ancestry: gitlinks move back and forth
  independently of the clone history the ff targets.

- **slot→main propagation** (before the parent ff). For each moved gitlink,
  fetch slot→main's clone and ff-merge to the recorded SHA. The detached HEAD a
  prior `submodule update` leaves must be **attached to a branch first**, else
  ff-only just moves HEAD and the ref keeps lagging. Runs *before* the parent ff
  so a failure leaves the parent at `main_before` and the next land retries the
  same `moved` set; the reverse order would strand the operator with `moved`
  empty and failed submodules unretried.

- **main→slot refresh** (after the parent ff). The mirror, inverted: when a
  parallel land bumped a submodule this slot didn't touch, the slot's parent
  tree now points past its submodule clone's HEAD and `git status` shows phantom
  rewinds. Fetch main→slot and ff. **Cosmetic** — main is already advanced — so
  failures only `WARN`; reverse-ordering it before the parent ff would gate the
  advance on a cosmetic concern.

`protocol.file.allow=always` threads through every `git submodule update` /
`fetch` above: pool slots' submodule clones use `file://` origins pointing at
the source's `.git/modules/...`, which git's default transport blocks.

## New submodules — populate from the slot

When a feature branch *introduces* a submodule, its gitlink shows as "moved" but
the main worktree has no clone to fast-forward. slot→main propagation can't act,
so it **collects** the path and skips; the populate below (run after the parent
ff) creates the clone. The pre-fix behaviour — `land_die "main worktree's
submodule clone missing at …"` — fired *before* the parent ff, stranding the
operator: slot branch advanced, `main` frozen at the pre-merge tip.

### Why not a plain `git submodule update --init`

The obvious populate reads the URL from the committed `.gitmodules`, i.e. the
**declared origin** (a GitHub remote, or a relative URL). Wrong here twice:

- it **reaches the network**, breaking land's local-only contract; and
- it **can't see slot-local submodule commits** — if the operator committed
  inside the new submodule before landing, the pinned SHA lives only in the
  slot's clone, so the checkout fails.

Acquire never writes mirror URLs into `.gitmodules`; it writes them to
`.git/config` (`src/submodules.rs`). The declared URL is all `--init` has, and
it isn't the right source.

### How it works

`_land_clone_sub_from_slot` (in `bin/wt`) clones main's copy **from the slot**,
mirroring acquire's two-phase config-write→update so the result is a normal
submodule with a stable origin:

```sh
declared=$(git -C <main> config -f .gitmodules --get submodule.<sub>.url)
git -C <main> submodule init -- <sub>                    # declared url → config
git -C <main> config submodule.<sub>.url <slot>/<sub>    # override to the local slot clone
git -C <main> -c protocol.file.allow=always submodule update -- <sub>   # clone from slot (local)
git -C <main> config submodule.<sub>.url "$declared"     # restore stable origin (super config)
git -C <main>/<sub> remote set-url origin "$declared"    # …and in the clone itself
```

`submodule update` checks out the gitlink SHA main's index records (the pinned
SHA after the ff), fetched from the slot which has it; origin is then restored
to the declared URL so the clone survives the slot being recycled. It runs
*after* the ff because `submodule init` needs main's working `.gitmodules` to
list the submodule, which only happens once the ff advances main's tree.

The populate is **non-fatal** — main is already correctly advanced, so a failure
emits a `land: WARN:` with the exact recovery command. A missing main clone is
populated the same way whether the submodule is brand-new or its clone was
deleted: the slot is authoritative either way, and populate-then-warn beats an
abort.

## Use cases

1. **No submodule changes** — no `160000` diff; nothing runs.
2. **Existing pointer moved** — warm fetch+attach+ff between the existing clones (propagation + refresh).
3. **Brand-new submodule introduced** — propagation collects it, main advances, the populate clones it from the slot.
4. **New submodule with slot-local commits** — covered: the populate sources from the slot, which has those commits (a plain `--init` would not).

## Non-goals

- No push / fetch-origin / PR — every fetch source is a local clone.
- No `submodule.recurse=true` — it fights the attach-to-branch design and mutates user repo config.
- Top-level only — nested submodules aren't auto-propagated or auto-populated.

## Pitfalls

- `git merge --ff-only` never populates submodule working trees — the populate must be an explicit step. ([gitsubmodules(7)](https://git-scm.com/docs/gitsubmodules))
- Plain `git submodule update` (no `--init`) silently skips unregistered submodules; the `submodule init` step registers the new one first. ([git-submodule(1)](https://git-scm.com/docs/git-submodule))
- The declared `.gitmodules` URL is the wrong source for a pool: remote, and lacking slot-local commits. Always source from the slot's clone.
- `set -u` + empty bash array: guard expansions with `${arr[@]+"${arr[@]}"}` (macOS ships bash 3.2). Collecting new submodules instead of spawning a subshell per `moved` entry leaves the propagation block's `pids`/`sub_logs` arrays empty on the all-new path — the guard is load-bearing.

## References

- [git-submodule(1)](https://git-scm.com/docs/git-submodule) — `update --init` registers + clones; plain `update` skips unregistered.
- [gitsubmodules(7)](https://git-scm.com/docs/gitsubmodules) — `merge --ff-only` does not populate submodule working trees.
- [Pro Git §Submodules](https://git-scm.com/book/en/v2/Git-Tools-Submodules) — submodule URL/config plumbing; empty-dir recovery via `update --init`.
