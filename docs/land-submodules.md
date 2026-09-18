# Land × submodules

> **Related:** [[wt.md#land-flow]] (the land step contract), [[submodules.md]] (the same problem at acquire time)

How `wt land` keeps git submodule clones in step with the gitlinks it moves
between the slot and main worktree. [[wt.md#land-flow]] lists *what* each step
does; this doc is the *why*.

Land is **local-only** — every fetch source is a sibling local clone
(`<slot>/<sub>` or `<main>/<sub>`), never `origin`. It is **top-level only**
(nested submodules are a documented v2; recover with `git submodule update
--init --recursive`). Submodule clones are checked out **detached at the pinned
commit** — no branch tracking, because each land fetches the *commit* (the other
side's `HEAD`), not a branch ref.

A submodule the acquire skipped via `worktreePoolTag` ([[submodules.md#filtering-worktreepooltag]])
has no clone in the slot, so every phase below passes over it — the gitlink lands
unchanged, which is what a filtered-out submodule should do.

## The phases

1. **Pre-stage sync** (before the auto-commit). Stock `git merge` (without
   `--recurse-submodules` — the IDE/`git pull` default) advances the recorded
   gitlink but leaves the submodule working dir at the pre-merge SHA; a blind
   `git add -u` would then re-stage that stale HEAD, *silently regressing the
   merge*. So when the working HEAD is an ancestor of the index gitlink, run
   `submodule update` to move the working dir forward; the reverse (working
   ahead) is the operator's real forward bump, left for the commit; no shared
   ancestor refuses as divergence. Keeps the slot's submodule `HEAD == the
   gitlink` that gets committed.

2. **slot→main advance** (before the parent ff). For each moved top-level
   gitlink whose clone main already has: fetch the slot submodule's `HEAD` (= the
   pin) into main's clone — **local, no origin** — then `merge --ff-only`. Done
   **before** the parent ff so a failure leaves `main` un-advanced and the re-run
   retries the same set, never stranding the operator half-landed. The ff-only
   *is* the divergence guard (refuses when main's clone holds commits the pin
   can't reach). Gitlinks with **no** main clone are newly introduced — deferred
   to phase 3.

3. **new-submodule populate** (after the parent ff). A brand-new submodule has no
   main clone to fast-forward; once the ff records its `.gitmodules` + gitlink,
   `_land_clone_sub_from_slot` clones it from the **slot's** clone (local) — see
   below. Non-fatal: main is already advanced, so a failure warns with the
   recovery command.

4. **main→slot refresh** (after the parent ff, cosmetic). When a parallel land
   bumped a submodule this slot didn't touch, the merge advanced the slot's
   parent-tree gitlink past its clone's HEAD and `git status` shows a phantom
   rewind. Fetch the pin from main's clone (local), then `submodule update`.
   Cosmetic — failures only `WARN`.

`protocol.file.allow=always` threads through every fetch / `submodule update`:
pool clones use `file://` origins (the source's `.git/modules/...`), which git's
default transport blocks.

## Why fetch `HEAD`, not a branch name

The fetch source is the *submodule's* clone, but the slot's branch (e.g.
`ignore-dll`) names a *superproject* ref. acquire only creates a same-named
branch inside a submodule when it exists at acquire time; a pin bump via detached
checkout, or a brand-new submodule, has no such ref. Fetching it gave `fatal:
couldn't find remote ref <branch>` → main's clone never got the new commit → the
old ancestry preflight reported a spurious **"diverged"**. Fetching `HEAD` —
which the pre-stage sync keeps equal to the pinned gitlink, and which always
exists — fixes it. (It was always a *local* fetch, so the contract was never
breached; only the message was alarming.)

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

The slot is the only source holding the pin *including any slot-local submodule
commits*. A plain `git submodule update --init` would clone from the declared
(remote) `.gitmodules` URL — reaching the network (breaking local-only) and
missing those commits. acquire never writes mirror URLs into `.gitmodules` (only
into `.git/config`, `src/submodules.rs`), so the declared URL is all `--init`
has, and it isn't the right source.

## Non-goals

- No push / fetch-origin / PR — every fetch is from a sibling local clone.
- No branch tracking — clones sit detached at the pin, since each land fetches
  the commit rather than a branch.
- No `submodule.recurse` / `--recursive` — nested submodules stay top-level-scoped.

## Pitfalls

Two that aren't visible from the phases above:

- `git merge --ff-only` never populates submodule working trees, which is why
  every advance and populate has to be explicit.
- `set -u` + an empty bash array needs `${arr[@]+"${arr[@]}"}` — macOS ships bash 3.2.

## References

- [git-submodule(1)](https://git-scm.com/docs/git-submodule) — `update --init` registers + clones; plain `update` skips unregistered.
- [gitsubmodules(7)](https://git-scm.com/docs/gitsubmodules) — `merge --ff-only` does not populate submodule working trees.
- [Pro Git §Submodules](https://git-scm.com/book/en/v2/Git-Tools-Submodules) — submodule URL/config plumbing; detached-HEAD checkout is normal.
