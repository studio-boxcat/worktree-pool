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

A **pool** is a fixed-cardinality set of slots backed by a single source repo. Slots are interchangeable git worktrees living at canonical paths (`{group}-{N}` or `slot-{N}`); each `acquire` picks an idle slot, writes a held marker, creates a branch named after the caller's `NAME`, and hands back the canonical path. `release` deletes the branch and removes the held marker. The slot dir never moves, so any cache that keys on absolute path (Unity Bee compile cache, watchman watches, IDE indexes) stays warm across recycles.

Pools are referenced by **key** (e.g. `myapp`, `another-pool`). Path: `$WORKTREE_ROOT/<key>/` — env var required, no fallback (set in `~/.zshenv.local`). For pools needing a different physical location (external SSD, etc.), symlink: `ln -s /Volumes/big/<key> "$WORKTREE_ROOT/<key>"`.

A **group** is an optional sub-namespace of slots (e.g. `ios`, `android`). With groups, idle slots are named `{group}-{N}`; without, just `slot-{N}`. Groups exist mainly for active-platform separation (e.g. Unity rebuilding `Library/` on iOS↔Android flip).

---

## Layout

```
$WORKTREE_ROOT/<key>/                                  # pool root
$WORKTREE_ROOT/<key>/.meta/config.yaml                # pool config (written by `init`)
$WORKTREE_ROOT/<key>/.meta/init/<slot-id>.lock        # init mutex (per-slot, flock-held)
$WORKTREE_ROOT/<key>/.meta/pool.lock                  # pool-wide mutex (acquire + release)
$WORKTREE_ROOT/<key>/{group}-{N}/                      # slot (always canonical; held iff marker present)
<source>/.git/worktrees/<git-id>/worktree-pool/lock   # held marker per slot
```

Slot dir stays at its canonical path for the lifetime of the pool — never renamed. Held/idle is signaled by the marker file's existence in the source repo's per-worktree gitdir. The user-given name lives in the git branch ref inside the slot (`git symbolic-ref --short HEAD`), not on disk; `release NAME` looks up the slot by branch.

Mutex files are advisory flocks (`std::fs::File::try_lock`, stable since Rust 1.89). The OS auto-releases on process death (SIGKILL / panic=abort / SIGHUP), so leftover files carry no semantic load — no PID tracking, heartbeat, or staleness threshold.

---

## Slot state

A slot is **held** iff the lock file exists; **idle** otherwise. (Transient post-crash states are reconciled by `reclaim_stale` — see [[lifecycle.md#crash-recovery]].) Lock body is line-oriented YAML, scalars only:

```yaml
started_at: 2026-05-05T03:34:56Z   # UTC, RFC3339; always present
full_sha: <40-char>                # always present (resolved at acquire)
group: ios                         # only if pool has groups configured
```

`started_at` is the source of truth for held-since; lock file mtime is the fallback when unparseable. `full_sha` enables same-SHA exclusion. `group` is informational (canonical group is derivable from the slot's path basename).

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
- `git` operations shell out via `git.rs`. Slot identity is the canonical path; the user-given name is just a branch ref. No rename, no `git worktree move`, no submodule admin self-heal.
- Atomic writes via `tempfile::NamedTempFile::persist` (handles EXDEV across volumes).
- Tests: `cargo test` (or `just test` to serialize). Unit + integration covering full lifecycle, race conditions, recycled-slot warmth, and crash recovery.

`just install` runs `cargo build --release` and symlinks `~/.local/bin/{worktree-pool,wt}` at the cargo artifact path (`target/release/worktree-pool`) and `bin/wt`. Re-running `cargo build --release` after edits updates the installed tool in place. No committed binary; `target/` stays gitignored.

---

## License

MIT.
