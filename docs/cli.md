# CLI reference

> **Related:** [[CLAUDE.md]], [[wt.md]] (dev-session helper), [[lifecycle.md]] (acquire/release invariants)

## Quick start

```sh
# Initialize a pool (or run `wt init --groups ios,android` from inside
# the source repo — auto-infers --source and --pool; --max-slots defaults to 16).
worktree-pool --pool myapp init \
  --source ~/Develop/myapp \
  --max-slots 16 \
  --groups ios,android

# Acquire a slot at a commit. Prints the canonical slot path (e.g.
# `$WORKTREE_ROOT/myapp/ios-0`) on stdout; NAME becomes the slot's branch ref.
worktree-pool --pool myapp acquire abc12345 --commit abc12345 --group ios

# Acquire at the pool's default_commit
worktree-pool --pool myapp acquire feature-x --group ios

# Release (looks up the held slot by branch name)
worktree-pool --pool myapp release abc12345

# Inspect
worktree-pool --pool myapp ls
worktree-pool --pool myapp ls --git-status     # adds dirty/untracked/ahead columns
worktree-pool --pool myapp inspect abc12345
worktree-pool --pool myapp path abc12345       # canonical slot path; exit 1 if not held
```

## CLI

```
worktree-pool --pool <key> init --source <repo> [--submodule-mirror-mode <source-submodules|bare-mirror> --submodule-mirror-base <p>] [--default-commit <ref>] --max-slots <n> [--groups <g1,g2>]
  # mirror flags are REQUIRED (together) when <repo> declares submodules — see [[lifecycle.md#submodule-mirror-mandatory-when-submodules-exist]]

worktree-pool --pool <key> acquire NAME [--commit <commitish>] [--group <g>] [--unique-sha] [--exclude-submodule-tags <t1,t2>]

worktree-pool --pool <key> release NAME
worktree-pool --pool <key> ls [--git-status]
worktree-pool --pool <key> inspect NAME
worktree-pool --pool <key> path NAME
worktree-pool --pool <key> unstick [--slot <id>]
worktree-pool --pool <key> validate-gitmodules

worktree-pool doctor
```

`NAME` is positional. For `acquire` it becomes the branch ref inside the chosen
slot; for `release`/`inspect`/`path` it's the lookup key — the tool scans held
slots and matches by branch name (see [[lifecycle.md#identity-model]]).

`acquire` fires the `wt_post_acquire` hook if the source ships `.wt-hooks.sh` —
fail-loud, so build pools get the same extension point as `wt go`. See
[[wt.md#hooks-sourcewt-hookssh]].

`unstick` is a read-only diagnostic: it reports whether each init mutex file is
currently locked by a live process. OS-managed flocks auto-release on process
death, so there's nothing to force-clear.

## Exit codes

Generic failures exit `1`. The codes below tag specific conditions so
retry-aware callers (CI build farms, supervisors) can branch on them:

| Code | Kind        | Meaning                                                          | Caller action          |
|------|-------------|------------------------------------------------------------------|------------------------|
| 0    | Success     | —                                                                | —                      |
| 1    | Generic     | Any other error (I/O, config, git failure, …)                    | Inspect stderr         |
| 2    | Usage       | Bad CLI shape (assigned by clap)                                 | Fix invocation         |
| 3    | Contended   | Every candidate slot's init mutex is held by another live acquire | Transient — retry      |
| 4    | Capacity    | Every slot in the requested group is held                        | `release` something    |
| 5    | UniqueSha   | `--unique-sha` matched an already-held slot                      | Reuse / release holder |

The contract is locked by `tests/lifecycle.rs::acquire_capacity_exhaustion` and
`unique_sha_refuses_second_acquire`. New conditions get new codes; existing
codes don't shift.

`doctor` is host-level (no `--pool`) and read-only. Checks: arch, `git --version`,
`$WORKTREE_ROOT` + pool count, binary quarantine xattr, and per-pool config +
source-path validation.

## Distribution

arm64 macOS only. Two tools end up on `$PATH`:

- `worktree-pool` — the Rust binary (the pool primitive).
- `wt` — Bash wrapper for the common dev-session lifecycle; auto-resolves pool
  key from cwd (override with `--pool`). Symlinked from `bin/wt`.

`scripts/install.sh` (also `just install`) builds via `cargo build --release` and
symlinks both into `~/.local/bin/` at the cargo artifact path, so any subsequent
`cargo build --release` updates the installed tool in place.

```sh
git clone https://github.com/studio-boxcat/worktree-pool.git ~/Develop/worktree-pool
cd ~/Develop/worktree-pool && just install
echo 'export WORKTREE_ROOT="$HOME/.worktree-pool"' >> ~/.zshenv.local
worktree-pool doctor
```
