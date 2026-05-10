# `wt` (dev-session helper)

> **Related:** [[../CLAUDE.md]], [[cli.md]] (underlying primitive), [[lifecycle.md]] (acquire/release invariants)

Bash dispatcher in `bin/wt`. Subcommands wrapping the slot + git-flow lifecycle for interactive dev work:

```sh
wt [--pool <key>] init    --max-slots <n> [pool-init-flags...]   # --source inferred from cwd
wt [--pool <key>] path    <name>     # print slot path; exit 0 if exists, 1 if not, 2 on error
wt [--pool <key>] go      <name> [--from <commit-ish>] [pool-acquire-flags...]
wt [--pool <key>] rm      <name> [--force]   # safety-checked release; --force discards dirty/unmerged
wt [--pool <key>] cleanup <name>     # 🟢/🟡/🔴 exit-trap classifier
wt [--pool <key>] ls      [--git-status]
wt [--pool <key>] info    <name>
wt land   [message]                  # ff slot's commits onto local main (local-only; no push, no PR)
wt whoami                            # cwd context: worktree|source|none + path
wt orient                            # print current repo path + CLAUDE.md
```

**Pool-key auto-resolution.** `--pool` is optional; `wt` infers the key from cwd:
1. If cwd is inside `$WORKTREE_ROOT/<key>/...` (a slot), key is the path segment.
2. Else `git rev-parse --show-toplevel` is matched against every pool's `source:` in `<pool>/.meta/config.yaml`. Unique match wins.
3. Zero matches or ambiguity → error listing candidates; operator passes `--pool <key>` explicitly.

This means inside `~/Develop/myapp` (the source repo), or inside any slot of that pool, every verb works without a pool-key argument. Each repo is its own pool, so ambiguity is rare in practice.

`init` is a thin pass-through to `worktree-pool init` that auto-infers `--source` from `git rev-parse --show-toplevel` and `--pool` from its basename. Run it from inside the source repo: `wt init --max-slots 16 --groups ios,android` (no need to type `--pool myapp --source ~/Develop/myapp`). Override the inferred key with `wt --pool <key> init ...`. All other init flags pass through unchanged. Refuses from inside an existing pool slot (toplevel would resolve to the slot, not the source).

`go` acquires/resumes a slot, prints a banner, `cd`s into the slot, runs `$WT_LAUNCHER -n <name>` (default `ai`) in `$SHELL`, and sets an EXIT trap that runs `wt cleanup`. The `-n <name>` is claude's session display name (visible in the prompt box, `/resume` picker, terminal title); assumes the configured launcher accepts it.

`--from <commit-ish>` (optional) forks the new branch from the given ref; translated to `acquire --commit <X>`. Omit to use the pool's `default_commit`. If a slot with `<name>` already exists, `go` resumes it with a loud warning (acquire flags are ignored on resume — slot stays at its existing commit); the underlying `ai` is invoked with `--continue`. Use `rm` first if you want to recreate from scratch.

`rm` is the manual one-shot with the same safety checks (refuse on dirty / unmerged); used directly when the session is gone but state persists. Pass `--force` (`-f`) to discard dirty tracked changes and/or unmerged commits — applies the work-loss waiver but still refuses 🔴 BROKEN slots (no git state to release through; recover those via `rm -rf`). `--force` is intentionally an `rm`-only knob, not a `cleanup` flag: `cleanup` is the auto-invoked exit-trap classifier whose entire purpose is preserving uncommitted work on shell exit, so a force-recycle there would defeat the trap. Operators discarding work always do so explicitly via `rm --force`.

`path` is the predicate query — prints `$WORKTREE_ROOT/<key>/<name>` on stdout (always, when key + pool are valid) and exits 0 if the slot exists, 1 if it doesn't, 2 on usage / pool-not-initialized. Lets consumer scripts branch on resume vs. fresh acquire (`if wt path … >/dev/null; then …`) without re-stitching the pool-root path themselves. Pattern matches `git rev-parse --git-dir`, `brew --prefix`, `pyenv prefix`.

`ls` filters `worktree-pool ls` to held slots only — operators almost always want "what's active right now," not idle capacity. `--git-status` is forwarded. Use `worktree-pool ls` directly for the full table including idle rows. `info` is a pass-through to `worktree-pool inspect --name <n>` with the inferred pool key prefilled.

`land` takes no pool key — it operates on the current worktree's repo and finds the main worktree via `git worktree list`. See [§Land flow](#land-flow) below.

`whoami` prints the cwd context for AI-agent bootstrap: `worktree <path>`, `source <path>`, or `none`. Path uses `~` for `$HOME`. Works in any cwd; never errors.

`orient` prints the current repo's toplevel path followed by its `CLAUDE.md` to stdout. Intended for AI-agent bootstrap — answers "where am I and what are the rules here." No pool-key argument; resolved from `git rev-parse`. Refuses if not inside a git repo.

## Hooks (`<source>/.wt-hooks.sh`)

If the source repo has a `.wt-hooks.sh` at its toplevel, `wt` sources it before each lifecycle verb runs. Define any subset of these bash functions to extend behavior; all are optional. Every hook sees `$WT_KEY`, `$WT_NAME`, `$WT_PATH`, `$WT_FRESH` (1=fresh acquire, 0=resume) in env.

| Function | When | Failure semantics |
|---|---|---|
| `wt_pre_go` | After acquire/resume, before launching the shell | Non-zero aborts launch (set -e); EXIT trap then fires `cleanup` |
| `wt_pre_rm` | After safety checks pass, before `release` | Best-effort (errors don't block release) |
| `wt_pre_cleanup` | Before the cleanup classifier runs | Best-effort |
| `wt_post_cleanup` | After classifier, only on the green/recycled path | Best-effort |
| `wt_post_land` | After `land` succeeds; sees `$WT_MAIN_BEFORE` / `$WT_MAIN_AFTER` (full SHAs) | Best-effort |

Hook scripts may also set the `WT_LAUNCHER` variable (default `ai`) to override the launcher invocation — receives `-n <name>` and `--continue` (on resume) appended.

Example minimal hooks file replacing the previous `WORKTREE_POOL_SESSION_*` pattern:

```bash
# <source>/.wt-hooks.sh
WT_LAUNCHER="ai --chrome"

wt_pre_go() {
  cd "$WT_PATH/app" && bun install --silent
}
wt_pre_rm()      { just _dev-stop "$WT_NAME" || true; }
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
| 🟢 | clean working tree AND 0 commits ahead local `main` | un-rename + delete branch + release (recycle) |
| 🟡 | dirty / untracked files | leave personalized — resume with `wt go` later |
| 🔴 UNMERGED | non-zero commits ahead local `main` (unmerged) | loud refuse — operator resolves before recycling |
| 🔴 BROKEN | slot dir exists but `<slot>/.git` is missing or dangling (ghost dir from partial cleanup debris) | loud refuse — operator recovers via `rm -rf` (the dir is unrecoverable as a worktree; nothing of value lives in `.git`-less debris) |

Re-running `wt go <name>` resumes any 🟡 / 🔴 UNMERGED slot. 🔴 BROKEN slots can't be resumed — `go` and `rm` both refuse them.

## Land flow

**Local-only — never fetches, never pushes, never opens a PR.** Lands the slot's commits onto local `refs/heads/main` via fast-forward. Idempotent re-run after manual conflict resolution. Auto-discovers the main worktree via `git worktree list`; no pool-key needed since the operation is git-only. Pushing to a remote (or opening a PR) is the operator's separate responsibility.

Steps in order, refuses loudly on anything unexpected:

1. Refuse unless on a non-`main` branch with no in-progress merge / rebase / cherry-pick / revert.
2. Refuse if any non-ignored untracked files exist in the current worktree (`git ls-files --others --exclude-standard`).
3. Find main via `git worktree list --porcelain`; refuse if `main` isn't checked out anywhere.
4. Refuse if main worktree has tracked uncommitted changes (untracked there is fine — `merge --ff-only` in step 8 refuses if untracked files would be overwritten, so operator scratch survives).
5. Auto-commit dirty tracked work with the supplied message. Refuses if dirty *and* no message. `wip` shorthand → `WIP via land`.
6. `git merge main`. No-op in the common case (main is ancestor of slot HEAD); slot's commits fast-forward main in step 9 — keeps history linear. A real 3-way merge only happens when a parallel slot advanced main first; halts on conflict, resolve + `git add` + `git commit`, then re-run land.
7. Refuse if `main` is no longer ancestor of `HEAD` (a parallel slot's land advanced main during long conflict resolution).
8. **Propagate submodule commits before parent advance.** For each top-level submodule whose gitlink moved between `<main_before>` and slot HEAD (`git diff --raw <main_before> <slot_head> | awk '$2=="160000"'`), `git -C <main_path>/<sub> fetch <slot_path>/<sub> <branch>`. Then if main_path/sub is detached, attach to a branch before the ff-merge so its ref tracks the gitlink instead of lagging (the typical post-`git submodule update` state is detached, and ff-only on detached HEAD only moves HEAD). Attach-target priority: `.gitmodules` `submodule.<path>.branch` if set and that branch exists in the submodule clone; else `main`; else skip the attach. Lookup assumes name==path (the `git submodule add` default). Skip the attach when on any branch (don't silently switch operator-managed branch state). Then `git -c core.hooksPath=/dev/null merge --ff-only <new_sha>`. Both checkout and ff-only refuse on untracked-collision — preserves operator scratch in actively-edited submodule clones. Skip when the slot's submodule is absent (tag-excluded at acquire — gate via `[ -e .../.git ]` since submodules' `.git` is a gitlink *file*, not a directory). Run all submodules in parallel via background subshells (Nx speedup at 7+ submodules; output interleaves). On any single failure, refuse loudly with main NOT advanced — re-running land after fixing retries cleanly: each submodule's fetch+attach+ff is idempotent, so already-advanced submodules no-op. (The reverse order — parent first — would strand the operator with `moved` empty next time.) Top-level only; nested-submodule propagation is a v2 (commits at nested levels remain on the slot's per-submodule `<slot-name>` branch from acquire's step 11, so manual `git -C <main>/<top>/<nested> fetch <slot>/<top>/<nested> <branch>` recovers them when needed).
9. `git -C <main_path> -c core.hooksPath=/dev/null merge --ff-only <slot_HEAD>` — advances `refs/heads/main` and refreshes main's working tree atomically. Pre-guarded by `git -C <main_path> symbolic-ref HEAD == refs/heads/main` so the operator manually checking out another branch in main_path can't silently fast-forward the wrong branch. `--ff-only` refuses on untracked-file collision (preserves operator scratch — `reset --hard` would silently delete) and on a parallel land that advanced main past our base. `core.hooksPath=/dev/null` suppresses `post-merge` so user repos don't get a surprise hook firing — `wt_post_land` is the documented extension point.

Idempotent: re-runs are safe (no-op if `main == HEAD`). Resume after conflict = re-run land after the manual merge commit lands. Pushing to a remote (or opening a PR) is the operator's separate responsibility — `wt land` only advances local refs.
