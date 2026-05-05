# worktree-pool

A recyclable pool of `git worktree` checkouts with named lifecycle, branch creation, and same-SHA exclusion. Each pool serves one source repo; multiple pools coexist on a host. Designed for CI build farms and dev session workflows where worktree caches (Unity `Library/`, `node_modules/`, gradle / xcode artifacts) should stay warm across acquires.

**Status:** v0.1 — early. arm64 macOS only.

---

## Concepts

A **pool** is a fixed-cardinality set of slots backed by a single source repo. Slots are interchangeable git worktrees; each `acquire` picks an idle slot, renames it to the caller's name, creates a branch, and hands back the path. `release` un-renames it back to the idle namespace and deletes the branch. Caches inside the slot dir survive recycling.

Pools are referenced by **key** (e.g. `myapp`, `another-pool`). Path is fixed: `~/.worktree-pool/<key>/`. For pools needing a different physical location (external SSD, etc.), symlink: `ln -s /Volumes/big/myapp ~/.worktree-pool/myapp`.

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

## Layout

```
~/.worktree-pool/<key>/                            — pool root
~/.worktree-pool/<key>/.meta/config.yaml          — pool config (written by `init`)
~/.worktree-pool/<key>/.meta/init/<slot-id>.lock  — init mutex (per-slot)
~/.worktree-pool/<key>/.meta/pool.lock            — pool-wide mutex (acquire + release)
~/.worktree-pool/<key>/{group}-{N}/                — idle slot
~/.worktree-pool/<key>/<name>/                     — held slot (post-rename)
<source>/.git/worktrees/<git-id>/worktree-pool/lock — held marker per slot
```

The held marker lives in the source repo's per-worktree gitdir (which stays stable across our `fs::rename` + `git worktree repair` flow — see "Why not `git worktree move`?" below). Slot dir stays pristine; `git status` inside a slot shows only the user's actual changes.

**Symlinked pool root constraint**: if `~/.worktree-pool/<key>` is a symlink (typical when relocating slots to a faster volume), the symlink basename **must match** the target directory name. Submodule `core.worktree` rewrites are anchored on the pool-key segment, derived from the symlink basename. A mismatch (`ln -s /Volumes/big/myapp-pool ~/.worktree-pool/myapp`) would silently no-op the rewrites. Standard form: `ln -s /Volumes/big/<key> ~/.worktree-pool/<key>`.

---

## Slot state

A slot is **held** iff the lock file exists; **idle** otherwise. Lock body (1-3 lines YAML, scalar values only):

```yaml
started_at: 2026-05-05T03:34:56Z   # UTC, RFC3339
full_sha: <40-char>                # always present (resolved at acquire time)
group: ios                         # only if pool has groups configured
```

`started_at` is the source of truth for held-since; lock file mtime is a fallback. `full_sha` enables same-SHA exclusion (refuse acquire when another slot holds the same full_sha). `group` enables un-rename namespace at release.

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

---

## CLI

```
worktree-pool --pool <key> init --source <repo> [--submodule-mirror-mode <m>] [--submodule-mirror-base <p>] [--default-commit <ref>] --max-slots <n> [--groups <g1,g2>]

worktree-pool --pool <key> acquire --name <n> [--commit <commitish>] [--group <g>] [--exclude-submodule-tags <t1,t2>]

worktree-pool --pool <key> release --name <n>
worktree-pool --pool <key> ls [--git-status]
worktree-pool --pool <key> inspect --name <n>
worktree-pool --pool <key> unstick [--slot <id>]
worktree-pool --pool <key> validate-gitmodules

worktree-pool doctor
```

---

## Lifecycle

### `acquire`

1. Resolve `--commit` (default `default_commit` from pool config) against source → full SHA.
2. Same-SHA exclusion: scan held locks; refuse if any has matching `full_sha`.
3. Pick idle `{group}-N` (smallest free N in that group, or `slot-N` if no groups).
4. Acquire per-slot init mutex (`<pool>/.meta/init/<slot-id>.lock`, `O_EXCL`); heartbeat every 30s during init.
5. If fresh slot (no `.git`): `git worktree add --detach <slot> <full_sha>`. If recycled: `git -C <slot> reset --hard <full_sha>` (NEVER `git clean` — untracked files are caller's warmth).
6. Write lock at `<source>/.git/worktrees/<id>/worktree-pool/lock` atomically (tempfile + rename) — held marker lands BEFORE the rename.
7. Rename slot via `fs::rename(<slot>, <name>) + git worktree repair <name>` plus per-submodule `core.worktree` rewrite (see "Why not `git worktree move`?").
8. Force-create branch via `git -C <name> update-ref refs/heads/<name> HEAD && git -C <name> symbolic-ref -m "worktree-pool acquire" HEAD refs/heads/<name>`. (Avoids `git checkout -B`'s per-file filter-process pings.)
9. Drop pool-wide mutex; per-slot init mutex still held.
10. `git -C <name> submodule update --init`-equivalent, two-phase: sequential `git config submodule.<name>.url` writes (URL overrides per `submodule_mirror_*`), then parallel per-submodule `git submodule update <path>`. Tag-excluded submodules filtered by `--exclude-submodule-tags` against `worktreePoolTag` in `.gitmodules`.
11. Release init mutex.
12. Print slot path on stdout.

### `release`

1. Acquire pool-wide mutex.
2. Locate slot by user-name (`<pool>/<name>`).
3. Delete lock file (held → idle visible to other acquires).
4. `git -C <name> checkout --detach`. Best-effort `git -C <name> branch -D <name>`. Best-effort `git -C <name> push origin --delete <name>` (no-op for bare-mirror sources; only meaningful if `origin` is a real remote).
5. Compute smallest free `{group}-N` (excluding any held slot's home id).
6. Un-rename via `fs::rename(<name>, <{group}-N>) + git worktree repair` plus submodule `core.worktree` self-heal rewrite.
7. Release pool-wide mutex.

### Same-SHA exclusion

When `acquire` resolves `--commit` to a full SHA, it scans all held locks. If any has a matching `full_sha`, the acquire is refused with an error naming the slot, lock holder name, and held-since age. Operator decides whether to wait, reuse the existing slot's output, or release the conflicting slot. This prevents two parallel builds from doing identical work.

### Init mutex liveness

The init mutex's mtime is updated every 30s by the holder during init (heartbeat). On acquire, if a slot's init mutex exists with mtime older than 60min, it's reclaimed silently with a stderr warning logged. This handles the SIGKILL'd-init edge case without time-based silent reaping of held slots. Operator can also `worktree-pool unstick [--slot <id>]` for explicit cleanup.

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

## Submodule filtering

The tool reads `worktreePoolTag = <tag>` lines in the source repo's `.gitmodules` (case-insensitive on key — git lowercases on read). `acquire --exclude-submodule-tags <t1,t2>` deinits any submodule whose tag matches and skips it during the submodule init/update phase. The taxonomy lives in `.gitmodules` (version-controlled with the source repo) so adding a new tagged submodule propagates to every consumer on next checkout.

Example:

```ini
# .gitmodules in source repo
[submodule "Packages/com.unity.ide.rider"]
    path = Packages/com.unity.ide.rider
    url = git@github.com:org/com.unity.ide.rider.git
    worktreePoolTag = editor
```

```sh
# CI build skips editor-only modules
worktree-pool --pool myapp acquire --name abc12345 --commit abc12345 --group ios --exclude-submodule-tags editor

# Dev session includes them
worktree-pool --pool myapp acquire --name feature-x --group ios
```

`worktree-pool --pool <key> validate-gitmodules` parses `.gitmodules` and warns on unknown `worktreePool*` keys (catches typos like `worktreePoolTags` plural).

---

## Distribution

arm64 macOS only. Two artifacts ship from `bin/`:

- `bin/worktree-pool-darwin-arm64` — the Rust binary (the pool primitive). Ad-hoc codesigned (`codesign --sign - --force`).
- `bin/worktree-pool-session` — Bash wrapper for the common dev-session lifecycle (acquire + interactive shell + safe-recycle on exit). Project-agnostic; takes `<pool-key>` as first arg.

`scripts/install.sh` (also `just install`) symlinks both into `~/.local/bin/`.

```sh
git clone https://github.com/studio-boxcat/worktree-pool.git ~/Develop/worktree-pool
cd ~/Develop/worktree-pool && just install
worktree-pool doctor
```

To rebuild the Rust binary: `just release-binary` (reproducible flags + codesign + stage). Commit the resulting `bin/worktree-pool-darwin-arm64`.

## `worktree-pool-session` (dev-session helper)

Three subcommands wrapping the lifecycle for interactive dev work:

```sh
worktree-pool-session go      <pool-key> <name> [pool-acquire-flags...]
worktree-pool-session cleanup <pool-key> <name>     # 🟢/🟡/🔴 exit-trap classifier
worktree-pool-session rm      <pool-key> <name>     # safety-checked release
```

`go` acquires/resumes a slot, prints a banner, `cd`s into the slot, runs `$WORKTREE_POOL_SESSION_CMD` (default `ai`) in `$SHELL`, and traps `cleanup` on exit. `rm` is the manual safety-checked release. `cleanup` classifies the slot's state on exit and either recycles or leaves it personalized:

| Marker | Condition | Action |
|---|---|---|
| 🟢 | working tree clean AND 0 commits ahead `origin/main` | un-rename + delete branch + release (auto-recycle) |
| 🟡 | dirty / untracked files | leave personalized — resume with `session go` later |
| 🔴 | non-zero commits ahead `origin/main` (unmerged) | loud refuse — operator resolves via `git push` / `branch -D` / merge-back-to-main |

The classifier always exits 0 so `wt-go`'s exit trap doesn't muddy the user's shell exit status. Re-running `session go <key> <name>` resumes any 🟡 / 🔴 slot.

Per-consumer integration is a thin `just` recipe pre-filling the pool key:

```bash
# myapp's justfile (just wt-go <name>)
wt-go name *flags:
    @worktree-pool-session go myapp {{quote(name)}} {{flags}}
wt-rm name:
    @worktree-pool-session rm myapp {{quote(name)}}
wt-cleanup name:
    @worktree-pool-session cleanup myapp {{quote(name)}}
```

---

## Recipe integration

Each consumer wraps the tool with a thin recipe that absorbs the `--pool` flag:

```bash
# myapp-ci/justfile
client-repo-pool *args:
    worktree-pool --pool myapp {{args}}
```

```bash
# another-pool/justfile
pool *args:
    worktree-pool --pool another-pool {{args}}
```

Recipes are host-agnostic — pool keys map to fixed paths under `~/.worktree-pool/<key>/`, so the same recipe runs on both server and laptop.

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

## What this tool does not do

- **No GC.** All cleanup is operator-explicit. Capacity-bound errors list the table; operator picks a slot to release.
- **No registry.** Pool key → path mapping is convention-based (`~/.worktree-pool/<key>/`). No external registry file, no env var.
- **No cross-host coordination.** Pools are host-local. Network-mounted shared pools are not supported (no `host`/`pid` liveness checks).
- **No dead-process detection.** A SIGKILL'd holder leaves the lock; operator notices via `ls` and runs `release`. The exception is the init mutex (60min stale → reclaim).
- **No auto-recovery.** Process crashes between rename and lock-write may leave orphans visible in `ls`; operator inspects and recovers manually.

These cuts keep the tool small and predictable. If you need GC, write a 5-line script: `ls --git-status` → filter → `release --name <n>` per match.

---

## Limits

- Branch refs accumulate in the source repo for SIGKILL'd builds and GC-style abandoned dev sessions (the latter intentional — work-recovery via `git branch | grep`). For high-volume CI, periodic `git for-each-ref --format='%(refname:short)' refs/heads/ | xargs -I X sh -c 'git merge-base --is-ancestor X origin/main && git branch -D X'` cleanup is the consumer's responsibility.
- Same-SHA exclusion is per-pool. Two pools sharing a source repo do not coordinate.
- `git status --porcelain` performance on huge worktrees (50k+ files) is the bottleneck for `ls --git-status`; plain `ls` is cheap (lock-mtime only).

---

## License

MIT.
