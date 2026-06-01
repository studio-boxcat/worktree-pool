# `wt` (dev-session helper)

> **Related:** [[CLAUDE.md]], [[cli.md]] (underlying primitive), [[lifecycle.md]] (acquire/release invariants)

Bash dispatcher in `bin/wt`. Subcommands wrapping the slot + git-flow lifecycle for interactive dev work:

```sh
wt [--pool <key>] init    [--max-slots <n>] [pool-init-flags...] # --source inferred from cwd; --max-slots defaults to 16
wt [--pool <key>] path    <name>     # print canonical slot path; resolves NAME via branch ref (or slot id fallback). Exit 0 if found, 1 if not, 2 on error
wt [--pool <key>] go      <name> [--from <commit-ish>] [pool-acquire-flags...]
wt [--pool <key>] release <name> [--force]   # safety-checked release; --force discards dirty/unmerged
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
1. If cwd is inside `$WORKTREE_ROOT/<key>/...` (a slot), key is the path segment.
2. Else `git rev-parse --show-toplevel` is matched against every pool's `source:` in `<pool>/.meta/config.yaml`. Unique match wins.
3. Zero matches or ambiguity → error listing candidates; operator passes `--pool <key>` explicitly.

This means inside `~/Develop/myapp` (the source repo), or inside any slot of that pool, every verb works without a pool-key argument. Each repo is its own pool, so ambiguity is rare in practice.

`init` is a thin pass-through to `worktree-pool init` that auto-infers `--source` from `git rev-parse --show-toplevel` and `--pool` from its basename. Run it from inside the source repo: `wt init --groups ios,android` (no need to type `--pool myapp --source ~/Develop/myapp`; `--max-slots` defaults to 16, override with `--max-slots <n>`). Override the inferred key with `wt --pool <key> init ...`. All other init flags pass through unchanged. Refuses from inside an existing pool slot (toplevel would resolve to the slot, not the source).

`go` acquires/resumes a slot, prints a banner, `cd`s into the slot, runs `$WT_LAUNCHER -n <name>` (default `ai`) in `$SHELL`, and sets an EXIT trap that runs an internal handler (which delegates to `wt cleanup` in the normal case, or preserves the slot on launcher misfire — see [§Cleanup classifier](#cleanup-classifier)). The `-n <name>` is claude's session display name (visible in the prompt box, `/resume` picker, terminal title); assumes the configured launcher accepts it.

**TTY guard.** `go` refuses early if stdin or stdout isn't a TTY. The launcher (`ai` / `claude`) needs an interactive terminal; without one it exits in <1s and the EXIT trap would silently 🟢-recycle a fresh slot — destroying the diagnostic surface. Common cause: running `wt go` from inside an existing AI session. For non-interactive automation use `worktree-pool acquire` directly, or set `WT_GO_ALLOW_NOTTY=1` to bypass the guard.

`--from <commit-ish>` (optional) forks the new branch from the given ref; translated to `acquire --commit <X>`. Omit to use the pool's `default_commit`. If a slot with `<name>` already exists, `go` resumes it with a loud warning (acquire flags are ignored on resume — slot stays at its existing commit); the underlying `ai` is invoked with `--continue`. Use `release` first if you want to recreate from scratch.

Acquire removes a leftover `git index.lock` in the slot it recycles, right before `reset --hard` — see [[lifecycle.md#crash-recovery]]. Within an active session, `wt land` likewise clears the current slot's `index.lock` before it commits. So a stuck `git status` / `git commit` reporting `Unable to create 'index.lock': File exists` (e.g. left by a crashed lazygit) clears on the next `wt land`, or when the released slot is later recycled.

`release` is the manual one-shot with the same safety checks (refuse on dirty / unmerged); used directly when the session is gone but state persists. Pass `--force` (`-f`) to discard dirty tracked changes and/or unmerged commits — applies the work-loss waiver but still refuses 🔴 BROKEN slots (no git state to release through; recover those via `rm -rf`). `--force` is intentionally a `release`-only knob, not a `cleanup` flag: `cleanup` is the auto-invoked exit-trap classifier whose entire purpose is preserving uncommitted work on shell exit, so a force-recycle there would defeat the trap. Operators discarding work always do so explicitly via `release --force`.

`path` is the predicate query — resolves NAME to a canonical slot path (`{group}-{N}` / `slot-{N}`). Resolution order: first the held-slot branch lookup (`worktree-pool path NAME`, which scans held slots for branch == NAME), then a literal canonical-id fallback (`$WORKTREE_ROOT/<key>/NAME` if NAME happens to be a slot id like `ios-0`). Exits 0 if found, 1 if not, 2 on usage / pool-not-initialized. Lets consumer scripts branch on resume vs. fresh acquire (`if wt path … >/dev/null; then …`). Pattern matches `git rev-parse --git-dir`, `brew --prefix`, `pyenv prefix`.

`ls` filters `worktree-pool ls` to held slots only — operators almost always want "what's active right now," not idle capacity. `--git-status` is **on by default** (DIRTY/UNTRK/AHEAD columns) — the dev-session wrapper is almost always asked "what state are my slots in?", and one `git status --porcelain` per held slot is cheap. `--bare` opts out for cold caches or very large slot dirs. The `NAME` column reads `(detached)` when HEAD isn't on a branch (operator ran `git checkout <sha>` or hand-deleted the slot's branch); detached slots still recycle on cleanup but the column flags the anomaly without `wt info`. Use `worktree-pool ls` directly for the full table including idle rows. `info` is a pass-through to `worktree-pool inspect <name>` with the inferred pool key prefilled.

`sweep` runs `cleanup` over every held slot in the pool — same classifier semantics, applied in a loop, with a final tally. Operator-driven (not automatic). Catches orphans whose EXIT trap never fired: a killed shell, a slot whose branch was hand-deleted (now detached HEAD with 0 ahead → 🟢 recycle), or a manual `git worktree add` that bypassed `wt go`. Always exits 0 — same trap-friendly contract as `cleanup` itself.

`land` takes no pool key — it operates on the current worktree's repo and finds the main worktree via `git worktree list`. See [§Land flow](#land-flow) below.

`whoami` prints the cwd context for AI-agent bootstrap: `worktree <path>`, `source <path>`, or `none`. Path uses `~` for `$HOME`. Works in any cwd; never errors.

`orient` prints the current repo's toplevel path followed by its `CLAUDE.md` to stdout. Intended for AI-agent bootstrap — answers "where am I and what are the rules here." No pool-key argument; resolved from `git rev-parse`. Refuses if not inside a git repo.

## Hooks (`<source>/.wt-hooks.sh`)

If the source repo has a `.wt-hooks.sh` at its toplevel, the named bash functions below extend lifecycle verbs. Define any subset; all are optional. Every hook sees `$WT_KEY`, `$WT_NAME`, `$WT_PATH` in env, plus `$WT_FRESH` — but note the two firers carry **different axes** in `WT_FRESH`: for `wt`-fired hooks it's acquire-vs-resume (`1`=fresh acquire, `0`=resume of an existing slot); for the core-fired `wt_post_acquire` it's slot-materialization (`1`=freshly created worktree with cold caches, `0`=recycled warm slot). Branch on it accordingly.

**Who fires what.** `wt_post_acquire` is fired by **worktree-pool core** (`src/hooks.rs`), so it runs for a direct `worktree-pool acquire` (CI / build pools) *and* on a fresh `wt go` acquire. It does **not** fire on `wt go` *resume* — resume reuses the existing slot without calling `acquire`. The rest are wrapper-only verbs (`go` shell-launch, `cleanup`, `land`) and are fired by `wt`, which sources `.wt-hooks.sh` before each. Core reads `.wt-hooks.sh` from the **acquired slot's checkout** (so it works for bare sources too and reflects the acquired commit's hook); `wt` sources it from the source worktree.

| Function | Fired by | When | Failure semantics |
|---|---|---|---|
| `wt_post_acquire` | core | After a slot is acquired (fresh `acquire`, not `wt go` resume), in the slot, before the path is printed; `$WT_FRESH`=1 means a freshly created worktree (cold caches) | **Non-zero fails the acquire** (no path emitted) |
| `wt_pre_go` | wt | After acquire/resume, before launching the shell | Non-zero aborts launch (set -e); EXIT trap then fires `cleanup` |
| `wt_pre_cleanup` | wt | Before the cleanup classifier runs | Best-effort |
| `wt_post_cleanup` | wt | After classifier, only on the green/recycled path | Best-effort |
| `wt_post_land` | wt | After `land` succeeds; sees `$WT_MAIN_BEFORE` / `$WT_MAIN_AFTER` (full SHAs) | Best-effort |

Hook scripts may also set the `WT_LAUNCHER` variable (default `ai`) to override the launcher invocation — receives `-n <name>` and `--continue` (on resume) appended.

Example minimal hooks file:

```bash
# <source>/.wt-hooks.sh
WT_LAUNCHER="ai --chrome"

# Fired by core on every fresh acquire (incl. direct `worktree-pool acquire`).
# Runs in the slot; gate first-time-only setup on a freshly created worktree.
# The body runs under `set -e` (a non-zero return REJECTS the acquire), so use
# an `if` block rather than `cond && cmd` — the latter returns 1 as the last
# statement on a recycled (WT_FRESH=0) slot and would fail every warm acquire.
# Write only to stderr; stdout is reserved for the slot path acquire emits.
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

Always exits 0 — it's an exit-trap target, so `wt go`'s exit trap doesn't muddy the user's shell exit status.

| Marker | Condition | Action |
|---|---|---|
| 🟢 | clean working tree AND 0 commits ahead local `main` | detach HEAD + delete branch (recycle in place) |
| 🟡 | dirty / untracked files | leave personalized — resume with `wt go` later |
| 🟡 misfire | fresh acquire AND launcher exited in <`LAUNCH_MISFIRE_SECS` (default 3) AND tree clean AND 0 ahead | leave in place; suggests `wt go <name>` to retry, `wt release --force <name>` to discard. Fired by the EXIT-trap handler before delegating to cleanup; preserves the slot when no real session ran. |
| 🔴 UNMERGED | non-zero commits ahead local `main` (unmerged) | loud refuse — operator resolves before recycling |
| 🔴 BROKEN | slot dir exists but `<slot>/.git` is missing or dangling (ghost dir from partial cleanup debris) | loud refuse — operator recovers via `rm -rf` (the dir is unrecoverable as a worktree; nothing of value lives in `.git`-less debris) |

Re-running `wt go <name>` resumes any 🟡 / 🟡 misfire / 🔴 UNMERGED slot. 🔴 BROKEN slots can't be resumed — `go` and `release` both refuse them.

## Land flow

**Local-only — never fetches, never pushes, never opens a PR.** Lands the slot's commits onto local `refs/heads/main` via fast-forward. Idempotent re-run after manual conflict resolution. Auto-discovers the main worktree via `git worktree list`; no pool-key needed since the operation is git-only. Pushing to a remote (or opening a PR) is the operator's separate responsibility.

Steps in order, refuses loudly on anything unexpected:

1. Refuse unless on a non-`main` branch with no in-progress merge / rebase / cherry-pick / revert in cwd.
2. **No-op early exit.** If HEAD ≡ `main` AND tree is clean AND no untracked AND no message argument → exit 0 silently before acquiring the lock or running any other scan. Lets operators verify "did I land?" without tripping refusals from unrelated in-progress state in main_path.
3. Refuse if any non-ignored untracked files exist in the current worktree (`git ls-files --others --exclude-standard`).
4. Find main via `git worktree list --porcelain`; refuse if `main` isn't checked out anywhere.
5. **Acquire per-source `land.lock`** at `<common-gitdir>/worktree-pool/land.lock` (one source ⇒ one in-flight land; different sources parallelize). PID-tagged file, reclaimed by next acquirer via PID-liveness (`kill -0`) or mtime fallback (5 min stale). Released by EXIT trap on every exit path including `die` and conflict-exit, so a long manual-resolution pause doesn't wedge other slots.
6. **Refuse on in-progress git operation in `<main_path>` or any of its top-level submodules.** Walks `.gitmodules` for the path list (`git submodule foreach` would recurse into nested, which the submodule advance below doesn't touch). Maps marker → recovery hint (`MERGE_HEAD` → `merge --abort`, `rebase-merge` → `rebase --abort`, etc.). Aggregated into one refusal listing all offenders.
7. Refuse if main worktree has tracked uncommitted changes (untracked there is fine — the parent `merge --ff-only` refuses if untracked files would be overwritten, so operator scratch survives).
8. **Pre-stage submodule sync, then auto-commit dirty tracked work.** Sync each submodule's working dir to its recorded gitlink (handles the post-merge-without-`--recurse-submodules` state where a blind `git add -u` would regress the gitlink); refuse on divergence. Then auto-commit dirty tracked work with the supplied message — refuses if dirty *and* no message (`wip` → `WIP via land`). See [[land-submodules.md#the-phases]].
9. `git merge main`. No-op in the common case (main is ancestor of slot HEAD); slot's commits fast-forward main below — keeps history linear. A real 3-way merge only happens when a parallel slot advanced main first; halts on conflict, resolve + `git add` + `git commit`, then re-run land. **Parent-order rewrite (always-on):** if HEAD moved AND it has exactly 2 parents, rebuild via `git commit-tree` with parents `(main, slot)`. Operator's commit message (`%B`), author identity (`%an`/`%ae`/`%aI`) and committer identity (`%cn`/`%ce`/`%cI`) are preserved verbatim — the rewrite is tree-identical so no labor was redone. Auto-generated `"Merge main into <branch>"` message is replaced with `"Merge <branch> into main"`; operator-authored resume messages survive. Skipped for no-op merges and octopus (3+ parents). Keeps `git log --first-parent main` on mainline; reflog journals the old tip.
10. Refuse if `main` is no longer ancestor of `HEAD` (a parallel slot's land advanced main during long conflict resolution — this only fires when the lock was released mid-flow, e.g. via the conflict-exit + resume path).
11. **Advance main's submodule clones to the new pins — before the parent ff.** For each top-level submodule whose gitlink moved between `<main_before>` and slot HEAD: fetch the slot submodule's `HEAD` (= the pin) into main's clone (local, no origin — fetching the superproject branch name was the old `couldn't find remote ref` bug), then `merge --ff-only`. Done *before* the parent advance so a failure — untracked collision, or main-side divergence, both of which ff-only refuses (subsuming the old ancestry preflight) — leaves `main` un-advanced and the re-run retries cleanly. Submodules with no main clone are newly introduced — collected for step 12's post-ff populate. Detached HEAD is fine; clones are not branch-tracked. Skip tag-excluded (no slot clone). Top-level only. See [[land-submodules.md]].
12. `git -C <main_path> -c core.hooksPath=/dev/null merge --ff-only <slot_HEAD>` — advances `refs/heads/main` and refreshes main's working tree atomically. Pre-guarded by `git -C <main_path> symbolic-ref HEAD == refs/heads/main` so the operator manually checking out another branch in main_path can't silently fast-forward the wrong branch. `--ff-only` refuses on untracked-file collision (preserves operator scratch — `reset --hard` would silently delete) and on a parallel land that advanced main past our base. `core.hooksPath=/dev/null` suppresses `post-merge` so user repos don't get a surprise hook firing — `wt_post_land` is the documented extension point.

    **Then populate main's clones of newly-introduced submodules** (those step 11 collected) — done after the ff (main's `.gitmodules` now lists them), cloning each from the slot's clone, fully local. **Non-fatal**: a failure emits `land: WARN:` with the recovery command. See [[land-submodules.md]] for why it sources from the slot, not the declared origin.
13. **Refresh slot's submodule working trees for gitlinks main brought in.** When a parallel land bumped a submodule this slot didn't touch, fetch the pin from main's clone (`HEAD`, local) then `git submodule update` so `git status` stops showing phantom rewinds. **Cosmetic** — main is already advanced, so failures only `WARN`. Top-level only. See [[land-submodules.md#the-phases]].

Idempotent: re-runs are safe (no-op early exit in step 2 covers the trivial case; mid-flow re-runs reach the same gates). Resume after conflict = re-run land after the manual merge commit lands. Pushing to a remote (or opening a PR) is the operator's separate responsibility — `wt land` only advances local refs.
