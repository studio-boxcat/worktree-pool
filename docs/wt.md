# `wt` (dev-session helper)

> **Related:** [[CLAUDE.md]], [[cli.md]] (underlying primitive), [[lifecycle.md]] (acquire/release invariants)

Bash dispatcher in `bin/wt`. Subcommands wrapping the slot + git-flow lifecycle for interactive dev work:

```sh
wt [--pool <key>] init    [--max-slots <n>] [pool-init-flags...] # --source inferred from cwd; --max-slots defaults to 16
wt [--pool <key>] path    <name>     # print canonical slot path; exit 0 found, 1 not, 2 on error
wt [--pool <key>] acquire <name> [--from <commit-ish>] [pool-acquire-flags...]   # acquire only, print path; no session
wt [--pool <key>] go      <name> [--from <commit-ish>] [pool-acquire-flags...]
wt [--pool <key>] release [name] [--force]   # safety-checked release; name defaults to cwd's slot; --force discards dirty/unmerged
wt [--pool <key>] cleanup <name>     # 🟢/🟡/🔴 exit-trap classifier
wt [--pool <key>] sweep              # run cleanup over every held slot in the pool
wt [--pool <key>] ls      [--bare]   # held slots + git status (DIRTY/UNTRK/AHEAD); --bare skips git calls
wt [--pool <key>] info    <name>
wt land   [message]                  # ff slot's commits onto local main (local-only; no push, no PR)
wt whoami                            # cwd context: worktree|source|none + path
wt orient                            # print current repo path + CLAUDE.md
wt help   [verb]                     # per-verb usage; also `wt <verb> --help`
```

**Pool-key auto-resolution.** `--pool` is optional; `wt` infers the key from cwd:
1. cwd inside `$WORKTREE_ROOT/<key>/...` (a slot) → key is that path segment.
2. else `git rev-parse --show-toplevel` matched against every pool's `source:` in `config.yaml`; unique match wins.
3. zero/ambiguous → error listing candidates; pass `--pool <key>`.

So inside the source repo or any of its slots, every verb works without a pool-key argument. Each repo is its own pool, so ambiguity is rare.

`init` passes through to `worktree-pool init`, auto-inferring `--source` from `git rev-parse --show-toplevel` and `--pool` from its basename (`--max-slots` defaults to 16). Run it from inside the source repo: `wt init --groups ios,android`. Refuses from inside a slot (toplevel would resolve to the slot, not the source).

`acquire` is the thin primitive: `worktree-pool acquire` with pool-key inference and `--from` (→ `--commit`) — creates a slot, prints its path, nothing else (no `cd`, launcher, or EXIT trap). Use `go` for the interactive flow.

`go` acquires/resumes a slot, prints a banner, `cd`s in, runs `$WT_LAUNCHER -n <name>` (default `ai`) in `$SHELL`, and sets an EXIT trap routing between misfire-preserve and `wt cleanup` (see [[#cleanup-classifier]]). `-n <name>` is the launcher's session display name. `--from <commit-ish>` (→ `acquire --commit`) forks the new branch from a ref; omit for `default_commit`. If `<name>` already exists, `go` resumes it with a loud warning (acquire flags ignored; `ai` invoked with `--continue`) — `release` first to recreate.

**TTY guard.** `go` refuses early if stdin/stdout isn't a TTY: the launcher exits in <1s without one, and the EXIT trap would silently 🟢-recycle the fresh slot, destroying the diagnostic surface. Common cause: running `wt go` from inside an existing AI session. For non-interactive automation use `worktree-pool acquire`, or set `WT_GO_ALLOW_NOTTY=1`.

Acquire and `wt land` both clear a leftover `git index.lock` in the active slot before writing the index (see [[lifecycle.md#crash-recovery]]). So a stuck `git status`/`git commit` reporting `Unable to create 'index.lock'` clears on the next `wt land` or when the slot is later recycled.

`release` is the manual one-shot with the same safety checks (refuse on dirty / unmerged). With no `name` it releases the slot cwd sits in (resolved via that slot's branch). `--force` (`-f`) discards dirty tracked changes and/or unmerged commits, but still refuses 🔴 BROKEN slots (recover via `rm -rf`). `--force` is intentionally `release`-only, not a `cleanup` flag: `cleanup` is the auto-invoked exit-trap classifier whose purpose is preserving uncommitted work, so force-recycle there would defeat the trap.

`path` resolves NAME to a canonical slot path: first the held-slot branch lookup (`worktree-pool path NAME`), then a literal canonical-id fallback (`$WORKTREE_ROOT/<key>/NAME`). Exits 0 found, 1 not, 2 on usage/pool-not-init. Lets consumer scripts branch on resume vs. fresh acquire (pattern matches `git rev-parse --git-dir`, `brew --prefix`).

`ls` filters `worktree-pool ls` to held slots — operators almost always want "what's active now." `--git-status` (DIRTY/UNTRK/AHEAD) is **on by default**; one `git status --porcelain` per held slot is cheap. `--bare` opts out for cold caches or huge slot dirs. The `NAME` column shows `(detached)` when HEAD isn't on a branch (operator checked out a SHA or hand-deleted the branch); such slots still recycle but the column flags the anomaly. `info` is a pass-through to `worktree-pool inspect <name>` with the pool key prefilled.

`sweep` runs `cleanup` over every held slot — same classifier semantics in a loop, with a final tally. Operator-driven. Catches orphans whose EXIT trap never fired (killed shell, hand-deleted branch → detached HEAD with 0 ahead → 🟢 recycle, manual `git worktree add`). Always exits 0.

`land` takes no pool key — operates on the current worktree's repo, finds main via `git worktree list`. See [[#land-flow]].

`whoami` prints the cwd context for AI-agent bootstrap: `worktree <path>`, `source <path>`, or `none` (`~` for `$HOME`). Works in any cwd; never errors. `orient` prints the current repo's toplevel path + its `CLAUDE.md` — "where am I and what are the rules here." Refuses outside a git repo.

## Hooks (`<source>/.wt-hooks.sh`)

If the source repo has a `.wt-hooks.sh` at its toplevel, the named bash functions below extend lifecycle verbs. All optional; define any subset. Every hook sees `$WT_KEY`, `$WT_NAME`, `$WT_PATH` plus `$WT_FRESH` — but the two firers carry **different axes** in `WT_FRESH`: `wt`-fired hooks use acquire-vs-resume (`1`=fresh acquire, `0`=resume); core-fired `wt_post_acquire` uses slot-materialization (`1`=freshly created worktree with cold caches, `0`=recycled warm slot).

**Who fires what.** `wt_post_acquire` is fired by **worktree-pool core** (`src/hooks.rs`), so it runs for a direct `worktree-pool acquire` (CI / build pools) *and* a fresh `wt go` acquire — but **not** `wt go` *resume* (resume reuses the slot without calling `acquire`). The rest are wrapper-only verbs fired by `wt`. Core reads `.wt-hooks.sh` from the **acquired slot's checkout** (works for bare sources, reflects the acquired commit's hook); `wt` sources it from the source worktree.

| Function | Fired by | When | Failure semantics |
|---|---|---|---|
| `wt_post_acquire` | core | After acquire (fresh, not `wt go` resume), in the slot, before the path is printed; `$WT_FRESH`=1 = freshly created worktree (cold caches) | **Non-zero fails the acquire** (no path emitted) |
| `wt_pre_go` | wt | After acquire/resume, before launching the shell | Non-zero aborts launch (set -e); EXIT trap then fires `cleanup` |
| `wt_pre_cleanup` | wt | Before the cleanup classifier runs | Best-effort |
| `wt_post_cleanup` | wt | After classifier, only on the green/recycled path | Best-effort |
| `wt_post_land` | wt | After `land` succeeds; sees `$WT_MAIN_BEFORE` / `$WT_MAIN_AFTER` (full SHAs) | Best-effort |

Hook scripts may also set `WT_LAUNCHER` (default `ai`) to override the launcher — receives `-n <name>` and `--continue` (on resume) appended.

Example minimal hooks file:

```bash
# <source>/.wt-hooks.sh
WT_LAUNCHER="ai --chrome"

# Fired by core on every fresh acquire (incl. direct `worktree-pool acquire`).
# Runs in the slot; gate first-time-only setup on a freshly created worktree.
# The body runs under `set -e` (non-zero return REJECTS the acquire), so use an
# `if` block, not `cond && cmd` — the latter returns 1 as the last statement on
# a recycled (WT_FRESH=0) slot and would fail every warm acquire. Write only to
# stderr; stdout is reserved for the slot path acquire emits.
wt_post_acquire() {
  if [ "$WT_FRESH" = 1 ]; then
    cp "$HOME/.config/myapp/local.env" "$WT_PATH/.env"
  fi
}
wt_pre_go() {
  cd "$WT_PATH/app" && bun install --silent
}
wt_pre_cleanup() { just _dev-stop "$WT_NAME" || true; }
wt_post_cleanup() { rm -f "$WT_PATH.log"; }
wt_post_land() {
  if [ -n "$(git diff --name-only "$WT_MAIN_BEFORE" "$WT_MAIN_AFTER" -- package.json bun.lock)" ]; then
    echo "deps changed — re-run bun install in active slots." >&2
  fi
}
```

## Cleanup classifier

Always exits 0 — it's an exit-trap target, so `wt go`'s trap doesn't muddy the user's shell exit status.

| Marker | Condition | Action |
|---|---|---|
| 🟢 | clean working tree AND 0 commits ahead local `main` | detach HEAD + delete branch (recycle in place) |
| 🟡 | dirty / untracked files | leave personalized — resume with `wt go` later |
| 🟡 misfire | fresh acquire AND launcher exited in <`LAUNCH_MISFIRE_SECS` (default 3) AND tree clean AND 0 ahead | leave in place; suggests `wt go <name>` to retry, `wt release --force <name>` to discard. Fired by the EXIT-trap handler before delegating to cleanup; preserves the slot when no real session ran |
| 🔴 UNMERGED | non-zero commits ahead local `main` | loud refuse — operator resolves before recycling |
| 🔴 BROKEN | slot dir exists but `<slot>/.git` is missing or dangling (ghost dir from partial cleanup) | loud refuse — operator recovers via `rm -rf` |

Re-running `wt go <name>` resumes any 🟡 / 🟡 misfire / 🔴 UNMERGED slot. 🔴 BROKEN slots can't be resumed — `go` and `release` both refuse them.

## Land flow

**Local-only — never fetches, never pushes, never opens a PR.** Lands the slot's commits onto local `refs/heads/main` via fast-forward; idempotent re-run after manual conflict resolution. Auto-discovers main via `git worktree list`. Pushing / opening a PR is the operator's separate responsibility. Why each submodule step does what it does: [[land-submodules.md]].

Steps in order, refusing loudly on anything unexpected:

1. Refuse unless on a non-`main` branch with no in-progress merge / rebase / cherry-pick / revert in cwd.
2. **No-op early exit.** HEAD ≡ `main` AND tree clean AND no untracked AND no message → exit 0 silently before acquiring the lock or any further scan. Lets operators verify "did I land?" without tripping refusals from unrelated in-progress state in main_path.
3. Refuse if any non-ignored untracked files exist in the current worktree.
4. Find main via `git worktree list --porcelain`; refuse if `main` isn't checked out anywhere.
5. **Acquire per-source `land.lock`** at `<common-gitdir>/worktree-pool/land.lock` (one source ⇒ one in-flight land; different sources parallelize). PID-tagged, reclaimed by next acquirer via `kill -0` liveness or mtime fallback (5 min stale). Released by EXIT trap on every exit path (incl. `die` and conflict-exit), so a long manual-resolution pause doesn't wedge other slots.
6. **Refuse on in-progress git operation in `<main_path>` or any top-level submodule.** Walks `.gitmodules` for the path list. Maps marker → recovery hint (`MERGE_HEAD` → `merge --abort`, etc.), aggregated into one refusal.
7. Refuse if main has tracked uncommitted changes (untracked there is fine — the parent `merge --ff-only` refuses if untracked files would be overwritten, so operator scratch survives).
8. **Pre-stage submodule sync, then auto-commit dirty tracked work.** Sync each submodule's working dir to its recorded gitlink (handles the post-merge-without-`--recurse-submodules` state where a blind `git add -u` would regress the gitlink); refuse on divergence. Then auto-commit with the supplied message — refuses if dirty *and* no message (`wip` → `WIP via land`). See [[land-submodules.md#the-phases]].
9. `git merge main`. No-op in the common case (main is ancestor of slot HEAD); slot's commits fast-forward main below — keeps history linear. A real 3-way merge only happens when a parallel slot advanced main first; halts on conflict (resolve + `git add` + `git commit`, then re-run). **Parent-order rewrite (always-on):** if HEAD moved and has exactly 2 parents, rebuild via `git commit-tree` with parents `(main, slot)`. Message (`%B`), author and committer identity are preserved verbatim — the rewrite is tree-identical. Auto-generated `"Merge main into <branch>"` becomes `"Merge <branch> into main"`; operator-authored resume messages survive. Skipped for no-ops and octopus (3+ parents). Keeps `git log --first-parent main` on mainline.
10. Refuse if `main` is no longer ancestor of `HEAD` (a parallel land advanced main during long conflict resolution — fires only when the lock was released mid-flow, e.g. conflict-exit + resume).
11. **Advance main's submodule clones to the new pins — before the parent ff.** For each top-level submodule whose gitlink moved between `<main_before>` and slot HEAD: fetch the slot submodule's `HEAD` (= the pin) into main's clone (local, no origin), then `merge --ff-only`. Done *before* the parent advance so a failure (untracked collision or main-side divergence, both refused by ff-only) leaves `main` un-advanced and the re-run retries cleanly. Submodules with no main clone are newly introduced — collected for step 12. Detached HEAD is fine; top-level only; tag-excluded ones skipped. See [[land-submodules.md]].
12. `git -C <main_path> -c core.hooksPath=/dev/null merge --ff-only <slot_HEAD>` — advances `refs/heads/main` and refreshes main's working tree atomically. Pre-guarded by `symbolic-ref HEAD == refs/heads/main` so a manually-checked-out other branch in main_path can't silently fast-forward. `--ff-only` refuses on untracked collision (preserves scratch) and on a parallel land that advanced main past our base. `core.hooksPath=/dev/null` suppresses `post-merge` — `wt_post_land` is the documented extension point.

    **Then populate main's clones of newly-introduced submodules** (step 11's collection) — after the ff (main's `.gitmodules` now lists them), cloning each from the slot's clone, fully local. **Non-fatal**: a failure emits `land: WARN:` with the recovery command. See [[land-submodules.md]] for why it sources from the slot.
13. **Refresh slot's submodule working trees for gitlinks main brought in.** When a parallel land bumped a submodule this slot didn't touch, fetch the pin from main's clone (local) then `git submodule update` so `git status` stops showing phantom rewinds. **Cosmetic** — main is already advanced, so failures only `WARN`. Top-level only. See [[land-submodules.md#the-phases]].

Idempotent: re-runs are safe (step 2 covers the trivial case; mid-flow re-runs reach the same gates). Resume after conflict = re-run land after the manual merge commit lands.
