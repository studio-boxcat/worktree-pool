# worktree-pool

A recyclable pool of `git worktree` checkouts with named lifecycle, branch creation, and same-SHA exclusion. Each pool serves one source repo; multiple pools coexist on a host. Designed for CI build farms and dev-session workflows where worktree caches (Unity `Library/`, `node_modules/`, gradle/xcode artifacts) should stay warm across acquires.

**Status:** v0.1 — early. arm64 macOS only.

This file is the contract. `README.md` is a symlink to it. Detail lives in `docs/`:

- [[cli.md]] — quick start, full CLI reference, install
- [[lifecycle.md]] — `acquire`/`release` invariants, crash recovery, same-SHA exclusion, submodule filtering, design rationale
- [[wt.md]] — `wt` dev-session helper: subcommands, hooks, cleanup classifier, land flow
- [[land-submodules.md]] — `wt land` × newly-introduced submodules: populate-from-slot rationale, local-only constraint
- [[integration.md]] — integration patterns, multi-slot gotchas, limits, scope cuts

Deferred work: [[TODO.md]].

---

## Concepts

A **pool** is a fixed-cardinality set of slots backed by a single source repo. Slots are interchangeable git worktrees at canonical paths (`{group}-{N}` or `slot-{N}`); `acquire` picks an idle one, brands it with the caller's `--lease` (which flips it to held), and prints its path. `release` reverses that. Slot id vs lease — the distinction the design turns on — is [[lifecycle.md#identity-model]].

Pools are referenced by **key** (e.g. `myapp`, `another-pool`). Path: `$WORKTREE_ROOT/<key>/` — env var required, no fallback (set in `~/.zshenv.local`). For pools needing a different physical location (external SSD, etc.), symlink: `ln -s /Volumes/big/<key> "$WORKTREE_ROOT/<key>"`.

A **group** is an optional sub-namespace of slots (e.g. `ios`, `android`). With groups, idle slots are named `{group}-{N}`; without, just `slot-{N}`. Groups exist mainly for active-platform separation (e.g. Unity rebuilding `Library/` on iOS↔Android flip).

---

## Layout

```
$WORKTREE_ROOT/<key>/                                  # pool root
$WORKTREE_ROOT/<key>/.meta/config.yaml                # pool config (written by `init`)
$WORKTREE_ROOT/<key>/.meta/init/<slot-id>.lock        # init mutex (per-slot, flock-held)
$WORKTREE_ROOT/<key>/.meta/pool.lock                  # pool-wide mutex (acquire + release)
$WORKTREE_ROOT/<key>/{group}-{N}/                      # slot (always canonical; held iff HEAD on branch)
<source-gitdir>/worktree-pool-config.lock             # per-source mutex (top-level submodule URL writes)
```

Held/idle is read from git's own HEAD state: **on a branch = held, detached = idle** (straight from the gitdir's `HEAD` file — no subprocess; `ref:` prefix = held, raw SHA = idle). No slot state lives on disk separately, so there is nothing to desync. Group is derivable from the slot's path basename.

Mutex files are advisory flocks (`std::fs::File::try_lock`, stable since Rust 1.89). The OS auto-releases on process death (SIGKILL / panic=abort / SIGHUP), so leftover files carry no semantic load — no PID tracking, heartbeat, or staleness threshold.

---

## Pool config (`<pool>/.meta/config.yaml`)

```yaml
schema_version: 1
source: ~/Develop/myapp
default_commit: refs/remotes/origin/main      # used when --commit omitted
max_slots: 16
groups: [ios, android]                         # optional; absent → slots named slot-{N}
submodule_mirror_mode: source-submodules             # bare-mirror | source-submodules
submodule_mirror_base: ~/Develop/myapp
```

`source` is the absolute path to the source git repo (bare or working clone). `submodule_mirror_*` rewrites submodule URLs to a **local mirror** (`source-submodules` or `bare-mirror`) at acquire time; a mirror is **mandatory** when the source declares submodules. Mode semantics, the deliberately-absent declared-URL fallback, and the init/acquire fail-loud gates: [[lifecycle.md#submodule-mirror-mandatory-when-submodules-exist]].

Per-host `init` runs once per pool key. Source path differs by host (build server's bare mirror vs laptop's working clone); pool config carries the host-specific values.

---

## Build / development

- Code lives in `src/`; one module per concern (e.g. `acquire`, `release`, `slot`, `mutex`, `submodules`, `parallel`, `dashboard`, `admin`, `doctor`, `exit`, `hooks` — `src/` is the source of truth). `parallel` wraps `std::thread::scope` with inline-fallback on OS thread-create failure (`Scope::spawn` panics under thread starvation; `panic = "abort"` would otherwise kill the process mid-release). Exposes `for_each`, `try_for_each`, and `map` (order-preserving collector). `exit` defines distinct exit codes for retry-aware callers — see [[cli.md#exit-codes]].
- Hand-rolled YAML in `yaml.rs` — line-oriented scalars only. `serde_yaml` is unmaintained; ~30 LOC suffices.
- `git` operations shell out via `git.rs`. Slot identity is the canonical path; the user-given name is just a branch ref. No rename, no `git worktree move`, no submodule admin self-heal.
- Atomic writes via `tempfile::NamedTempFile::persist` (handles EXDEV across volumes).
- Tests: `cargo test` or `just test` (pre-builds the binary; tests are parallel-safe — each owns a unique pool key + isolated temp `WORKTREE_ROOT`). Unit + integration covering full lifecycle, race conditions, recycled-slot warmth, and crash recovery.
- Docs: a `lefthook` pre-commit hook runs `md-orphan CLAUDE.md` — flags orphaned docs, broken links, and broken `[[wiki]]` anchors. Needs `lefthook` + `md-orphan` on `$PATH` and a one-time `lefthook install` per clone; exclusions live in `.md-orphan`.

`just install` runs `cargo build --release` and symlinks `~/.local/bin/{worktree-pool,wt}` at the cargo artifact path (`target/release/worktree-pool`) and `bin/wt`. Re-running `cargo build --release` after edits updates the installed tool in place. No committed binary; `target/` stays gitignored.

---

## License

MIT.
