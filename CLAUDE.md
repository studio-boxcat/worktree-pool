# worktree-pool

A recyclable pool of `git worktree` checkouts with named lifecycle, branch creation, and same-SHA exclusion. Each pool serves one source repo; multiple pools coexist on a host. Designed for CI build farms and dev-session workflows where worktree caches (Unity `Library/`, `node_modules/`, gradle/xcode artifacts) should stay warm across acquires.

**Status:** v0.1 — early. arm64 macOS only.

This file is the contract. `README.md` is a symlink to it. Detail lives in `docs/`:

- [[docs/cli.md]] — quick start, full CLI reference, install
- [[docs/lifecycle.md]] — `acquire`/`release` invariants, crash recovery, same-SHA exclusion, submodule filtering, design rationale
- [[docs/wt.md]] — `wt` dev-session helper: subcommands, hooks, cleanup classifier, land flow
- [[docs/integration.md]] — integration patterns, multi-slot gotchas, limits, scope cuts

---

## Concepts

A **pool** is a fixed-cardinality set of slots backed by a single source repo. Slots are interchangeable git worktrees; each `acquire` picks an idle slot, renames it to the caller's name, creates a branch, and hands back the path. `release` un-renames it back to the idle namespace and deletes the branch. Caches inside the slot dir survive recycling.

Pools are referenced by **key** (e.g. `myapp`, `another-pool`). Path: `$WORKTREE_ROOT/<key>/` — env var required, no fallback (set in `~/.zshenv.local`). For pools needing a different physical location (external SSD, etc.), symlink: `ln -s /Volumes/big/<key> "$WORKTREE_ROOT/<key>"`.

A **group** is an optional sub-namespace of slots (e.g. `ios`, `android`). With groups, idle slots are named `{group}-{N}`; without, just `slot-{N}`. Groups exist mainly for active-platform separation (e.g. Unity rebuilding `Library/` on iOS↔Android flip).

---

## Layout

```
$WORKTREE_ROOT/<key>/                                  # pool root
$WORKTREE_ROOT/<key>/.meta/config.yaml                # pool config (written by `init`)
$WORKTREE_ROOT/<key>/.meta/init/<slot-id>.lock        # init mutex (per-slot)
$WORKTREE_ROOT/<key>/.meta/pool.lock                  # pool-wide mutex (acquire + release)
$WORKTREE_ROOT/<key>/{group}-{N}/                      # idle slot
$WORKTREE_ROOT/<key>/<name>/                           # held slot (post-rename)
<source>/.git/worktrees/<git-id>/worktree-pool/lock   # held marker per slot
```

The held marker lives in the source repo's per-worktree gitdir (which stays stable across our `fs::rename` + `git worktree repair` flow — see [[docs/lifecycle.md#why-not-git-worktree-move]]). Slot dir stays pristine; `git status` inside a slot shows only the user's actual changes.

**Symlinked pool root constraint**: if `$WORKTREE_ROOT/<key>` is a symlink (typical when relocating slots to a faster volume), the symlink basename **must match** the target directory name. Submodule `core.worktree` rewrites are anchored on the pool-key segment, derived from the symlink basename. A mismatch (`ln -s /Volumes/big/myapp-pool "$WORKTREE_ROOT/myapp"`) would silently no-op the rewrites. Standard form: `ln -s /Volumes/big/<key> "$WORKTREE_ROOT/<key>"`.

---

## Slot state

A slot is **held** iff the lock file exists; **idle** otherwise. (Transient post-crash states are reconciled by `reclaim_stale` — see [[lifecycle.md#crash-recovery]].) Lock body is line-oriented YAML, scalars only:

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

## Build / development

- Code lives in `src/`; one module per concern (`acquire`, `release`, `slot`, `lock`, `mutex`, `submodules`, `parallel`, `dashboard`, `admin`, `doctor`). `parallel` wraps `std::thread::scope` with inline-fallback on OS thread-create failure — `Scope::spawn` panics under thread starvation, and `panic = "abort"` would otherwise kill the process mid-release.
- Hand-rolled YAML in `yaml.rs` — line-oriented scalars only. `serde_yaml` is unmaintained; ~30 LOC suffices.
- `git` operations shell out via `git.rs`. We bypass `git worktree move` entirely (refuses on slots with submodules — see [[docs/lifecycle.md#why-not-git-worktree-move]]); `worktree_rename` does `fs::rename` + `git worktree repair` + submodule admin `core.worktree` self-heal instead.
- Atomic writes via `tempfile::NamedTempFile::persist` (handles EXDEV across volumes).
- Tests: `cargo test` (or `just test` to serialize). Unit + integration covering full lifecycle, race conditions, recycled-slot warmth, and submodule-rewrite self-heal regression.

`just install` runs `cargo build --release` and symlinks `~/.local/bin/{worktree-pool,wt}` at the cargo artifact path (`target/release/worktree-pool`) and `bin/wt`. Re-running `cargo build --release` after edits updates the installed tool in place. No committed binary; `target/` stays gitignored.

---

## License

MIT.
