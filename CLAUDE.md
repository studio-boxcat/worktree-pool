# worktree-pool

Recyclable git-worktree pool with named lifecycle, branch creation, and same-SHA exclusion. Used as a primitive by CI build farms and dev-session workflows where worktree caches (Unity `Library/`, `node_modules/`, gradle/xcode artifacts) should stay warm across acquires.

User-facing docs in `README.md`. This file is the contract — what callers and integrators rely on.

---

## Layout

```
~/.worktree-pool/<key>/                                # pool root (fixed convention)
~/.worktree-pool/<key>/.meta/config.yaml              # written by `init`
~/.worktree-pool/<key>/.meta/init/<slot-id>.lock      # per-slot init mutex
~/.worktree-pool/<key>/.meta/pool.lock                # pool-wide mutex
~/.worktree-pool/<key>/{group}-{N}/                    # idle slot
~/.worktree-pool/<key>/<name>/                         # held slot (post-rename)
<source>/.git/worktrees/<git-id>/worktree-pool/lock   # held marker per slot
```

Pool key → path is `~/.worktree-pool/<key>/`. No registry, no env var. Operators symlink to relocate (`ln -s /Volumes/big/<key> ~/.worktree-pool/<key>`).

---

## Lock body (held marker)

YAML scalars only, line-oriented. `started_at` always present; `full_sha` always present (resolved at acquire); `group` only when pool has groups configured.

```yaml
started_at: 2026-05-05T03:34:56Z
full_sha: <40-char>
group: ios
```

Lock file mtime = held-since (fallback when `started_at` is unparseable). Mere file presence = held.

---

## Lifecycle invariants

**Acquire:**
1. Resolve `--commit` (default `default_commit` from config) against source repo → full SHA.
2. Take pool-wide mutex.
3. If `--unique-sha`, scan held locks for matching `full_sha`; refuse on hit.
4. Check capacity (`count_held_in_group >= max_slots` → refuse).
5. Iterate acquirable Ns (fresh + recycled-idle, smallest first). Try per-slot init mutex on each; first success wins.
6. Materialize: fresh → `git worktree add --detach <pool>/{group}-N <full_sha>`; recycled → `git -C <slot> reset --hard <full_sha>`. **Never `git clean`** — untracked files are caller's warmth.
7. Write lock at `<source>/.git/worktrees/<id>/worktree-pool/lock` (atomic).
8. `git worktree move <pool>/{group}-N <pool>/<name>`.
9. `git -C <pool>/<name> checkout -B <name>`.
10. `submodule update --init`, applying `--exclude-submodule-tags` filter + URL overrides per pool config.
11. Drop pool-wide mutex; print path on stdout (last line).

**Release:**
1. Take pool-wide mutex.
2. Read lock to recover `group`.
3. Delete lock (slot becomes idle to other acquires inside the mutex).
4. Detach HEAD; `branch -D <name>` (local); `push --delete origin <name>` (best-effort, no-op if never pushed).
5. Find smallest free `{group}-N`.
6. `git worktree move <pool>/<name> <pool>/{group}-N`.
7. Drop mutex.

**Crash recovery:** writing the lock BEFORE the rename means a crash leaves the slot held at canonical `{group}-N` (clean recovery state). A crash between rename and lock-write would leave a renamed slot with no lock — operator clears via `git -C <source> worktree remove --force <slot>` and re-acquires.

**Init mutex liveness:** mtime-heartbeated every 30s during init; reclaimable by another acquire after 60min of no heartbeat (covers SIGKILL'd cold submodule clones). Manual cleanup via `worktree-pool --pool <key> unstick`.

---

## Same-SHA exclusion

Opt-in via `--unique-sha` on acquire. Caller asserts "I'm doing duplicate-detectable work; refuse if another slot is already on this SHA." Build callers (CI) opt in; dev callers don't (devs branching off `main` don't want to fight a CI build at the same SHA).

The check holds the pool-wide mutex, so it's atomic w.r.t. other acquires. Cross-pool exclusion is not a thing — each pool is its own bucket.

---

## `worktreePoolTag` convention

Submodule taxonomy lives in source repo's `.gitmodules` (version-controlled, propagates on next checkout):

```ini
[submodule "Packages/com.unity.ide.rider"]
    path = Packages/com.unity.ide.rider
    url = ...
    worktreePoolTag = editor
```

`acquire --exclude-submodule-tags <t1,t2>` deinits + skips submodules whose tag matches. `validate-gitmodules` warns on misspelled `worktreePool*` keys (catches `worktreePoolTags` plural typo etc).

Tag filter applies at top level only; nested submodules always init when their parent is included.

---

## `worktree-pool-session` (dev-session helper)

Bash dispatcher in `bin/worktree-pool-session`. Three subcommands wrapping the lifecycle for interactive dev work:

```sh
worktree-pool-session go      <pool-key> <name> [pool-acquire-flags...]
worktree-pool-session cleanup <pool-key> <name>     # 🟢/🟡/🔴 exit-trap classifier
worktree-pool-session rm      <pool-key> <name>     # safety-checked release
```

Cleanup classifier (always exits 0 — it's an exit-trap target):

| Marker | Condition | Action |
|---|---|---|
| 🟢 | clean working tree AND 0 commits ahead `origin/main` | un-rename + delete branch + release (recycle) |
| 🟡 | dirty / untracked files | leave personalized — resume with `session go` later |
| 🔴 | non-zero commits ahead `origin/main` (unmerged) | loud refuse — operator resolves before recycling |

`rm` is the manual one-shot with the same safety checks (refuse on dirty / unmerged); used directly when the session is gone but state persists.

Pool key is the first positional → project-agnostic. Each consumer wraps via a thin `just` recipe pre-filling the key:

```bash
# myapp's justfile (just wt-go <name>)
wt-go name *flags:
    @worktree-pool-session go myapp {{quote(name)}} {{flags}}
```

Generic; same shape works for any pool.

---

## Multi-slot gotchas

Slots share `.git/` and `.git/modules/` with the source repo, but **not** the working tree. Things to know when running multiple slots concurrently:

- **Per-slot warmth lives inside the slot dir.** Build artifacts (Unity `Library/`, `Temp/`, `proj-*`, `node_modules/`, gradle caches) survive recycle because pool's `acquire` does `git reset --hard` only — never `git clean`. Across-platform flips inside a single slot rebuild platform-specific caches; don't symlink caches across slots.
- **Submodule git-dirs (`<source>/.git/modules/...`) are shared.** Concurrent submodule updates can race on ref locks; the pool's `retryOnGitLock` handles transient `index.lock` / `worktrees.lock` contention.
- **Shared docs (`TODO.md`, `CLAUDE.md`, `docs/`) are high-traffic.** Keep edits scoped, commit in their own commit, rebase early. A long-held dev session diverging on these is the usual source of conflicts.
- **Branch refs accumulate in the source repo.** `release` deletes the branch (local + remote best-effort), so steady state has zero buildup. Crashed acquires that bypass release leave orphans — operator can prune via `git for-each-ref refs/heads/ | xargs ...`.

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
wt-ls:
    @worktree-pool --pool myapp ls
wt-info name:
    @worktree-pool --pool myapp inspect --name {{quote(name)}}
```

Per-host `init` runs once per pool key. Source path differs by host (build server's bare mirror vs laptop's working clone); pool config carries the host-specific values.

## What this tool does NOT do

Cuts that simplify the design:

- **No GC.** Capacity-bound errors require explicit `release --name <n>`. Operator decides.
- **No registry.** Pool key → path is convention (`~/.worktree-pool/<key>/`). No `~/.config/...`-tracked file.
- **No host/pid liveness.** A dead worker's slot blocks until manually cleared. The exception is the init mutex (60min stale → reclaim). Cross-host pool sharing is unsupported.
- **No `--fresh` / `--volatile` flags.** Caller wipes warmth itself if needed; release is the only "give back" verb.

If you need GC-like behavior, write a 5-line script: `worktree-pool ls` → filter → `release --name <X>` per match. Keeps the binary lean.

---

## Build / development

- Code lives in `src/`; one module per concern (`acquire`, `release`, `slot`, `lock`, `mutex`, `submodules`, `dashboard`, `admin`, `doctor`).
- Hand-rolled YAML in `yaml.rs` — line-oriented scalars only. `serde_yaml` is unmaintained; ~30 LOC suffices.
- `git` operations shell out via `git.rs` (libgit2 doesn't expose `worktree move`).
- Atomic writes via `tempfile::NamedTempFile::persist` (handles EXDEV across volumes).
- Tests: `cargo test` (or `just test` to serialize). 26 unit + 12 integration covering full lifecycle, race conditions, and recycled-slot warmth regression.

`just release-binary` rebuilds the committed arm64 binary at `bin/worktree-pool-darwin-arm64` (reproducible flags + ad-hoc codesign). Commit the result.
