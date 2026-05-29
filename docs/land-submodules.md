# Land × submodules

> **Related:** [[wt.md#land-flow]] (the land step contract), [[lifecycle.md#submodule-filtering-worktreepooltag]] (acquire-time clone + URL rewrite)

How `wt land` keeps git submodule clones in step with the gitlinks it moves
between the slot's worktree and the main worktree. [[wt.md#land-flow]] lists
*what* each land step does; this doc explains *why*.

Land is **local-only** — every fetch source is a sibling local clone
(`<slot>/<sub>` or `<main>/<sub>`), never `origin`. It is **top-level only**
(nested submodules are a documented v2; recover with `git submodule update
--init --recursive`). It checks submodule clones out **detached at the pinned
commit** — no branch tracking, because each land fetches the *commit* (the
other side's `HEAD`), not a branch ref.

## The phases

1. **Pre-stage sync** (before the auto-commit). Stock `git merge` (without
   `--recurse-submodules`, the IDE/`git pull` default) advances the recorded
   gitlink but leaves the submodule working dir at the pre-merge SHA; a blind
   `git add -u` would then re-stage that stale HEAD, *silently regressing the
   merge*. So when a submodule's working HEAD is an ancestor of its index
   gitlink, run `submodule update` to move the working dir forward; the reverse
   (working ahead of index) is the operator's real forward bump, left for the
   commit; no shared ancestor refuses as divergence. This keeps the slot's
   submodule `HEAD == the gitlink` that gets committed.

2. **slot→main advance** (before the parent ff). For each moved top-level
   gitlink whose clone main already has: fetch the slot submodule's `HEAD` (=
   the pin) into main's clone — **local, no origin** — then `merge --ff-only` to
   it. Done **before** the parent fast-forward so a failure (untracked
   collision, or main-side divergence) leaves `main` un-advanced and the re-run
   retries the same set — never stranding the operator half-landed. The ff-only
   *is* the divergence guard (it refuses when main's clone holds commits the pin
   can't reach). Gitlinks with **no** main clone are newly introduced — deferred
   to phase 3.

3. **new-submodule populate** (after the parent ff). A brand-new submodule has
   no main clone to fast-forward; once the ff records its `.gitmodules` + gitlink,
   `_land_clone_sub_from_slot` clones it from the **slot's** clone (local) — see
   below. Non-fatal: main is already advanced, so a failure warns with the
   recovery command.

4. **main→slot refresh** (after the parent ff, cosmetic). When a parallel land
   bumped a submodule this slot didn't touch, the merge advanced the slot's
   parent-tree gitlink past its submodule clone's HEAD and `git status` shows a
   phantom rewind. Fetch the pin from main's clone (`HEAD`, local), then
   `submodule update` to check it out. Cosmetic — main is already advanced — so
   failures only `WARN`.

`protocol.file.allow=always` threads through every fetch / `submodule update`:
pool clones use `file://` origins pointing at the source's `.git/modules/...`,
which git's default transport blocks.

## Why fetch `HEAD`, not a branch name

The fetch source is the *submodule's* clone, but the slot's branch (e.g.
`ignore-dll`) names a *superproject* ref. acquire only creates a same-named
branch *inside* each submodule when it exists at acquire time and the pin is
advanced by committing on that branch; a pin bump via detached checkout, or a
brand-new submodule, has no such ref. Fetching it gave `fatal: couldn't find
remote ref <branch>` → main's clone never received the new commit → the old
ancestry preflight reported a spurious **"diverged"** (the pin-bump failure).
Fetching `HEAD` — which the pre-stage sync keeps equal to the pinned gitlink,
and which always exists — fixes it. (It was always a *local* fetch, so the
contract was never actually breached; only the message was alarming.)

## New submodules — populate from the slot

`_land_clone_sub_from_slot` (in `bin/wt`) clones main's copy **from the slot**,
mirroring acquire's two-phase config-write→update so the result is a normal
submodule with a stable origin:

```sh
declared=$(git -C <main> config -f .gitmodules --get submodule.<sub>.url)
git -C <main> submodule init -- <sub>                    # declared url → config
git -C <main> config submodule.<sub>.url <slot>/<sub>    # override to the local slot clone
git -C <main> -c protocol.file.allow=always submodule update -- <sub>   # clone from slot (local)
git -C <main> config submodule.<sub>.url "$declared"     # restore stable origin
git -C <main>/<sub> remote set-url origin "$declared"    # …and in the clone itself
```

The slot is the only source that holds the pin *including any slot-local
submodule commits*. A plain `git submodule update --init` would clone from the
declared (remote) `.gitmodules` URL — reaching the network (breaking local-only)
and missing those commits. acquire never writes mirror URLs into `.gitmodules`
(only into `.git/config`, `src/submodules.rs`), so the declared URL is all
`--init` has, and it isn't the right source.

## Use cases

1. **No submodule changes** — no `160000` diff; nothing runs.
2. **Existing pin bumped** — fetch the pin (local) + ff main's clone, before the parent ff.
3. **Brand-new submodule introduced** — deferred past the ff, then cloned from the slot.
4. **New submodule with slot-local commits** — covered: the populate sources from the slot, which has them.
5. **Parallel land bumped a sub this slot didn't touch** — cosmetic refresh of the slot's clone.

## Non-goals

- No push / fetch-origin / PR — every fetch is from a sibling local clone.
- No branch tracking — submodule clones sit detached at the pin (each land
  fetches the commit, not a branch). Dropped the attach-to-branch machinery that
  only existed to support fetch-by-branch.
- No `submodule.recurse` / `--recursive` — nested submodules stay top-level-scoped.

## Pitfalls

- `git merge --ff-only` never populates submodule working trees — populate/advance must be explicit.
- The submodule clone lacks the superproject branch ref — fetch `HEAD` (the pin), not the branch name.
- Advance submodules **before** the parent ff: a post-ff advance that fails would leave `main` advanced with no clean re-run.
- The declared `.gitmodules` URL is remote and lacks slot-local commits — clone new submodules from the slot, not the URL.
- `set -u` + empty bash array: guard expansions with `${arr[@]+"${arr[@]}"}` (macOS ships bash 3.2).

## References

- [git-submodule(1)](https://git-scm.com/docs/git-submodule) — `update --init` registers + clones; plain `update` skips unregistered.
- [gitsubmodules(7)](https://git-scm.com/docs/gitsubmodules) — `merge --ff-only` does not populate submodule working trees.
- [Pro Git §Submodules](https://git-scm.com/book/en/v2/Git-Tools-Submodules) — submodule URL/config plumbing; detached-HEAD checkout is normal.
