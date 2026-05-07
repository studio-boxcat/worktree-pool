# worktree-pool

A recyclable pool of `git worktree` checkouts with named lifecycle, branch creation, and same-SHA exclusion. Each pool serves one source repo; multiple pools coexist on a host. Designed for CI build farms and dev-session workflows where worktree caches (Unity `Library/`, `node_modules/`, gradle/xcode artifacts) should stay warm across acquires.

**Status:** v0.1 — early. arm64 macOS only.

This file is both the user-facing README and the contract — what callers and integrators rely on. `README.md` is a symlink to this file.

---

## Concepts

A **pool** is a fixed-cardinality set of slots backed by a single source repo. Slots are interchangeable git worktrees; each `acquire` picks an idle slot, renames it to the caller's name, creates a branch, and hands back the path. `release` un-renames it back to the idle namespace and deletes the branch. Caches inside the slot dir survive recycling.

Pools are referenced by **key** (e.g. `myapp`, `another-pool`). Path is fixed: `~/.worktree-pool/<key>/`. No registry, no env var. For pools needing a different physical location (external SSD, etc.), symlink: `ln -s /Volumes/big/<key> ~/.worktree-pool/<key>`.

A **group** is an optional sub-namespace of slots (e.g. `ios`, `android`). With groups, idle slots are named `{group}-{N}`; without, just `slot-{N}`. Groups exist mainly for active-platform separation (e.g. Unity rebuilding `Library/` on iOS↔Android flip).

---

## Quick start

```sh
# Initialize a pool
worktree-pool --pool myapp init \
  --source ~/Develop/myapp \
  --max-slots 16 \
  --groups ios,android

# Acquire a slot at a specific commit
worktree-pool --pool myapp acquire --name abc12345 --commit abc12345 --group ios
# → prints worktree path on stdout

# Acquire a dev session at origin/main (default)
worktree-pool --pool myapp acquire --name feature-x --group ios

# Release
worktree-pool --pool myapp release --name abc12345

# Inspect
worktree-pool --pool myapp ls
worktree-pool --pool myapp ls --git-status     # adds dirty/untracked/ahead columns
worktree-pool --pool myapp inspect --name abc12345
```

---

## CLI

```
worktree-pool --pool <key> init --source <repo> [--submodule-mirror-mode <m>] [--submodule-mirror-base <p>] [--default-commit <ref>] --max-slots <n> [--groups <g1,g2>]

worktree-pool --pool <key> acquire --name <n> [--commit <commitish>] [--group <g>] [--unique-sha] [--exclude-submodule-tags <t1,t2>]

worktree-pool --pool <key> release --name <n>
worktree-pool --pool <key> ls [--git-status]
worktree-pool --pool <key> inspect --name <n>
worktree-pool --pool <key> unstick [--slot <id>]
worktree-pool --pool <key> validate-gitmodules

worktree-pool doctor
```

---

## Layout

```
~/.worktree-pool/<key>/                                # pool root
~/.worktree-pool/<key>/.meta/config.yaml              # pool config (written by `init`)
~/.worktree-pool/<key>/.meta/init/<slot-id>.lock      # init mutex (per-slot)
~/.worktree-pool/<key>/.meta/pool.lock                # pool-wide mutex (acquire + release)
~/.worktree-pool/<key>/{group}-{N}/                    # idle slot
~/.worktree-pool/<key>/<name>/                         # held slot (post-rename)
<source>/.git/worktrees/<git-id>/worktree-pool/lock   # held marker per slot
```

The held marker lives in the source repo's per-worktree gitdir (which stays stable across our `fs::rename` + `git worktree repair` flow — see [§Why not `git worktree move`?](#why-not-git-worktree-move)). Slot dir stays pristine; `git status` inside a slot shows only the user's actual changes.

**Symlinked pool root constraint**: if `~/.worktree-pool/<key>` is a symlink (typical when relocating slots to a faster volume), the symlink basename **must match** the target directory name. Submodule `core.worktree` rewrites are anchored on the pool-key segment, derived from the symlink basename. A mismatch (`ln -s /Volumes/big/myapp-pool ~/.worktree-pool/myapp`) would silently no-op the rewrites. Standard form: `ln -s /Volumes/big/<key> ~/.worktree-pool/<key>`.

---

## Slot state

A slot is **held** iff the lock file exists; **idle** otherwise. Lock body is line-oriented YAML, scalars only:

```yaml
started_at: 2026-05-05T03:34:56Z   # UTC, RFC3339; always present
full_sha: <40-char>                # always present (resolved at acquire)
group: ios                         # only if pool has groups configured
```

`started_at` is the source of truth for held-since; lock file mtime is the fallback when unparseable. `full_sha` enables same-SHA exclusion. `group` enables un-rename namespace at release.

---

## Pool config (`<pool>/.meta/config.yaml`)

```yaml
schema_version: 1
source: ~/Develop/myapp
default_commit: refs/remotes/origin/main      # used when --commit omitted
max_slots: 16
groups: [ios, android]                         # optional; absent → slots named slot-{N}
submodule_mirror_mode: git-modules             # bare-mirror | git-modules; optional
submodule_mirror_base: ~/Develop/myapp
```

`source` is the absolute path to the source git repo (bare or working clone). `submodule_mirror_*` rewrites submodule URLs to local mirrors at acquire time (avoids GitHub fetch); both bare-mirror (`<base>/<orgRepo>.git`) and git-modules (`<source>/.git/modules/<composedName>`) modes supported. Omit if submodules use their declared URLs.

Per-host `init` runs once per pool key. Source path differs by host (build server's bare mirror vs laptop's working clone); pool config carries the host-specific values.

---

## Lifecycle invariants

### `acquire`

1. Resolve `--commit` (default `default_commit` from config) against source repo → full SHA.
2. Take pool-wide mutex.
3. If `--unique-sha`, scan held locks for matching `full_sha`; refuse on hit.
4. Check capacity (`count_held_in_group >= max_slots` → refuse with the slot table inline).
5. Iterate acquirable Ns (fresh + recycled-idle, smallest first). Try per-slot init mutex on each (`O_EXCL`); first success wins. Heartbeat mtime every 30s during init.
6. Materialize: fresh → `git worktree add --detach <pool>/{group}-N <full_sha>`; recycled → `git -C <slot> reset --hard <full_sha>`. **Never `git clean`** — untracked files are caller's warmth.
7. Write lock at `<source>/.git/worktrees/<id>/worktree-pool/lock` (atomic; tempfile + rename) — held marker lands BEFORE the rename.
8. Rename via `fs::rename` + `git worktree repair` (`git worktree move` refuses on slots with submodules — see `git.rs::worktree_rename`). Also rewrites every submodule admin's `core.worktree` (anchored on the pool-key segment, idempotent self-heal).
9. Force-create branch: `git -C <pool>/<name> update-ref refs/heads/<name> HEAD && symbolic-ref HEAD refs/heads/<name>`. (Avoids `git checkout -B`'s 600ms of per-file filter-process pings on a tree that's already at the right state.)
10. Drop pool-wide mutex (slot is now visibly held under user-name; submodule init below is per-slot work guarded by the still-held init mutex).
11. Submodule update, two-phase to dodge `<source>/.git/config` lockfile contention: (a) sequential `git config submodule.<name>.url` writes per submodule, applying URL overrides per pool config; (b) parallel per-submodule `git submodule update <path>` via `std::thread::scope`. Tag exclusion via `--exclude-submodule-tags` against `worktreePoolTag` in `.gitmodules`.
12. Release init mutex; print path on stdout (last line).

### `release`

1. Take pool-wide mutex.
2. Read lock to recover `group`.
3. Delete lock (slot becomes idle to other acquires inside the mutex).
4. Detach HEAD; `branch -D <name>` (local); `push --delete origin <name>` (best-effort, no-op if `origin` is a bare mirror).
5. Find smallest free `{group}-N`.
6. Un-rename via `fs::rename` + `git worktree repair` + submodule `core.worktree` self-heal (same primitives as acquire's rename).
7. Drop mutex.

### Crash recovery

Writing the lock BEFORE the rename means a crash leaves the slot held at canonical `{group}-N` (clean recovery state). A crash between rename and lock-write would leave a renamed slot with no lock — operator clears via `git -C <source> worktree remove --force <slot>` and re-acquires.

### Init mutex liveness

mtime-heartbeated every 30s during init; reclaimable by another acquire after 60min of no heartbeat (covers SIGKILL'd cold submodule clones), with a stderr warning logged. Manual cleanup via `worktree-pool --pool <key> unstick [--slot <id>]`.

### Capacity-bound failures

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

---

## `worktree-pool-session` (dev-session helper)

Bash dispatcher in `bin/worktree-pool-session`. Subcommands wrapping the slot + git-flow lifecycle for interactive dev work:

```sh
worktree-pool-session path    <pool-key> <name>     # print slot path; exit 0 if exists, 1 if not, 2 on error
worktree-pool-session go      <pool-key> <name> [--from <commit-ish>] [pool-acquire-flags...]
worktree-pool-session sync    [message]             # commit + merge origin/main + publish
worktree-pool-session cleanup <pool-key> <name>     # 🟢/🟡/🔴 exit-trap classifier
worktree-pool-session rm      <pool-key> <name>     # safety-checked release
worktree-pool-session orient                        # print current repo path + CLAUDE.md
```

`go` acquires/resumes a slot, prints a banner, `cd`s into the slot, runs `$WORKTREE_POOL_SESSION_CMD` (default `ai`) in `$SHELL`, and traps `cleanup` on exit. `--from <commit-ish>` (optional) forks the new branch from the given ref; translated to `acquire --commit <X>`. Omit to use the pool's `default_commit`. If a slot with `<name>` already exists, `go` resumes it with a loud warning (acquire flags are ignored on resume — slot stays at its existing commit); the underlying `ai` is invoked with `--continue`. Use `rm` first if you want to recreate from scratch. (Legacy alias `new` is accepted with a deprecation warning.)

`rm` is the manual one-shot with the same safety checks (refuse on dirty / unmerged); used directly when the session is gone but state persists.

`path` is the predicate query — prints `~/.worktree-pool/<key>/<name>` on stdout (always, when key + pool are valid) and exits 0 if the slot exists, 1 if it doesn't, 2 on usage / pool-not-initialized. Lets consumer recipes branch on resume vs. fresh acquire (`if path … >/dev/null; then …`) without re-stitching the pool-root path themselves. Pattern matches `git rev-parse --git-dir`, `brew --prefix`, `pyenv prefix`.

`sync` takes no pool-key — it operates on the current worktree's repo and finds the main worktree via `git worktree list`. See [§Sync flow](#sync-flow) below.

`orient` prints the current repo's toplevel path followed by its `CLAUDE.md` to stdout. Intended for AI-agent bootstrap — answers "where am I and what are the rules here." No pool-key argument; resolved from `git rev-parse`. Refuses if not inside a git repo.

Cleanup classifier (always exits 0 — it's an exit-trap target, so `wt-go`'s exit trap doesn't muddy the user's shell exit status):

| Marker | Condition | Action |
|---|---|---|
| 🟢 | clean working tree AND 0 commits ahead `origin/main` | un-rename + delete branch + release (recycle) |
| 🟡 | dirty / untracked files | leave personalized — resume with `session go` later |
| 🔴 | non-zero commits ahead `origin/main` (unmerged) | loud refuse — operator resolves before recycling |

Re-running `session go <key> <name>` resumes any 🟡 / 🔴 slot.

### Sync flow

Strict merge-back-to-`main` + publish. Idempotent re-run after manual conflict resolution. Auto-discovers the main worktree via `git worktree list`; no pool-key needed since the operation is git-only.

Steps in order, refuses loudly on anything unexpected:

1. Refuse unless on a non-`main` branch with no in-progress merge / rebase / cherry-pick / revert.
2. Refuse if any non-ignored untracked files exist in the current worktree (`git ls-files --others --exclude-standard`).
3. Find main via `git worktree list --porcelain`; refuse if `main` isn't checked out anywhere.
4. Refuse if main worktree has tracked uncommitted changes (untracked there is fine — `git reset --hard HEAD` only touches tracked files).
5. Auto-commit dirty tracked work with the supplied message. Refuses if dirty *and* no message. `wip` shorthand → `WIP via sync`.
6. `git fetch origin main`; `git merge --no-ff origin/main`. Halts on conflict — resolve, `git add`, `git commit`, then re-run sync (the merge step becomes a no-op).
7. Re-fetch `origin/main`; refuse if it's no longer ancestor of `HEAD` (someone advanced main during long conflict resolution).
8. Atomic `git update-ref refs/heads/main HEAD <expected_main>`. Refuses if a parallel slot advanced main first.
9. `git -C <main_path> reset --hard HEAD` to refresh main's tree.
10. `git push --atomic origin main <branch>` — both refs together or neither. No `--force`.

Idempotent: re-runs after a push reject are safe (no-op if `main == HEAD`). Resume after conflict = re-run sync after the manual merge commit lands.

---

## Integration patterns

Each consumer wraps the pool with thin recipes pre-filling the pool key. `worktree-pool-session` is project-agnostic; pool config (source path, mirror mode) lives in `<pool>/.meta/config.yaml` written once by `init`.

```bash
# Consumer's justfile, e.g. myapp
wt-go name *flags:
    @worktree-pool-session go myapp {{quote(name)}} {{flags}}
wt-rm name:
    @worktree-pool-session rm myapp {{quote(name)}}
wt-cleanup name:
    @worktree-pool-session cleanup myapp {{quote(name)}}
wt-path name:
    @worktree-pool-session path myapp {{quote(name)}}
wt-ls:
    @worktree-pool --pool myapp ls
wt-info name:
    @worktree-pool --pool myapp inspect --name {{quote(name)}}
wt-sync message="":
    @worktree-pool-session sync {{quote(message)}}
```

Pool key is the first positional for slot-scoped verbs → project-agnostic. Same shape works for any pool. Recipes are host-agnostic — pool keys map to fixed paths under `~/.worktree-pool/<key>/`, so the same recipe runs on both server and laptop.

---

## Multi-slot gotchas

Slots share `.git/` and `.git/modules/` with the source repo, but **not** the working tree. Things to know when running multiple slots concurrently:

- **Per-slot warmth lives inside the slot dir.** Build artifacts (Unity `Library/`, `Temp/`, `proj-*`, `node_modules/`, gradle caches) survive recycle because pool's `acquire` does `git reset --hard` only — never `git clean`. Across-platform flips inside a single slot rebuild platform-specific caches; don't symlink caches across slots.
- **Submodule git-dirs (`<source>/.git/modules/...`) are shared.** Concurrent submodule updates can race on ref locks (`index.lock` / `worktrees.lock`); git's own internal `O_EXCL` retry handles transient contention.
- **Shared docs (`TODO.md`, `CLAUDE.md`, `docs/`) are high-traffic.** Keep edits scoped, commit in their own commit, rebase early. A long-held dev session diverging on these is the usual source of conflicts.
- **Branch refs accumulate in the source repo.** `release` deletes the branch (local + remote best-effort), so steady state has zero buildup. Crashed acquires that bypass release leave orphans — operator can prune via `git for-each-ref refs/heads/ | xargs ...`.
- **LFS endpoint routing is consumer's responsibility.** Pool slots clone submodules from the source bare; if those submodules use LFS, smudging hits whatever `lfs.url` resolves to. Consumers using a remote LFS relay (e.g. EC2 reverse-proxy) should set a `[url] insteadOf` rewrite to a faster local endpoint where available. macmini-side example: `git config --global url.http://localhost:3690/.insteadOf https://relay.example/` (lives in `~/.gitconfig.local`, not the dotfiles repo, so it's machine-specific). Without the rewrite, cold acquires that hit LFS smudge incur per-object WAN round-trips. Pool tooling itself doesn't inspect or enforce this.

---

## What this tool does NOT do

Cuts that simplify the design:

- **No GC.** All cleanup is operator-explicit. Capacity-bound errors list the table; operator picks a slot to release.
- **No registry.** Pool key → path mapping is convention-based (`~/.worktree-pool/<key>/`). No `~/.config/...`-tracked file, no env var.
- **No cross-host coordination.** Pools are host-local. Network-mounted shared pools are not supported (no `host`/`pid` liveness checks).
- **No dead-process detection.** A SIGKILL'd holder leaves the lock; operator notices via `ls` and runs `release`. The exception is the init mutex (60min stale → reclaim).
- **No auto-recovery.** Process crashes between rename and lock-write may leave orphans visible in `ls`; operator inspects and recovers manually.
- **No `--fresh` / `--volatile` flags.** Caller wipes warmth itself if needed; release is the only "give back" verb.

If you need GC-like behavior, write a 5-line script: `worktree-pool ls` → filter → `release --name <X>` per match. Keeps the binary lean.

---

## Limits

- Branch refs accumulate in the source repo for SIGKILL'd builds and GC-style abandoned dev sessions (the latter intentional — work-recovery via `git branch | grep`). For high-volume CI, periodic `git for-each-ref --format='%(refname:short)' refs/heads/ | xargs -I X sh -c 'git merge-base --is-ancestor X origin/main && git branch -D X'` cleanup is the consumer's responsibility.
- Same-SHA exclusion is per-pool. Two pools sharing a source repo do not coordinate.
- `git status --porcelain` performance on huge worktrees (50k+ files) is the bottleneck for `ls --git-status`; plain `ls` is cheap (lock-mtime only).

---

## Distribution

arm64 macOS only. Two artifacts ship from `bin/`:

- `bin/worktree-pool-darwin-arm64` — the Rust binary (the pool primitive). Ad-hoc codesigned (`codesign --sign - --force`).
- `bin/worktree-pool-session` — Bash wrapper for the common dev-session lifecycle. Project-agnostic; takes `<pool-key>` as first arg.

`scripts/install.sh` (also `just install`) symlinks both into `~/.local/bin/`.

```sh
git clone https://github.com/studio-boxcat/worktree-pool.git ~/Develop/worktree-pool
cd ~/Develop/worktree-pool && just install
worktree-pool doctor
```

---

## Build / development

- Code lives in `src/`; one module per concern (`acquire`, `release`, `slot`, `lock`, `mutex`, `submodules`, `dashboard`, `admin`, `doctor`).
- Hand-rolled YAML in `yaml.rs` — line-oriented scalars only. `serde_yaml` is unmaintained; ~30 LOC suffices.
- `git` operations shell out via `git.rs`. We bypass `git worktree move` entirely (it refuses on slots with submodules, the common case) — `worktree_rename` does `fs::rename` + `git worktree repair` + submodule admin `core.worktree` self-heal instead.
- Atomic writes via `tempfile::NamedTempFile::persist` (handles EXDEV across volumes).
- Tests: `cargo test` (or `just test` to serialize). Unit + integration covering full lifecycle, race conditions, recycled-slot warmth, and submodule-rewrite self-heal regression.

`just release-binary` rebuilds the committed arm64 binary at `bin/worktree-pool-darwin-arm64` (reproducible flags + ad-hoc codesign). Commit the result.

---

## License

MIT.
