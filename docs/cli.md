# CLI reference

> **Related:** [[../CLAUDE.md]], [[wt.md]] (dev-session helper), [[lifecycle.md]] (acquire/release invariants)

## Quick start

```sh
# Initialize a pool (or run `wt init --groups ios,android` from inside
# the source repo — auto-infers --source and --pool; --max-slots defaults to 16).
worktree-pool --pool myapp init \
  --source ~/Develop/myapp \
  --max-slots 16 \
  --groups ios,android

# Acquire a slot at a specific commit
worktree-pool --pool myapp acquire --name abc12345 --commit abc12345 --group ios
# → prints worktree path on stdout

# Acquire a dev session at the pool's default_commit
worktree-pool --pool myapp acquire --name feature-x --group ios

# Release
worktree-pool --pool myapp release --name abc12345

# Inspect
worktree-pool --pool myapp ls
worktree-pool --pool myapp ls --git-status     # adds branch/dirty/untracked/ahead columns
worktree-pool --pool myapp inspect --name abc12345
```

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

`doctor` is host-level (no `--pool`) and read-only. Checks: arch, `git --version`, `$WORKTREE_ROOT` + pool count, binary quarantine xattr, and stale `index.lock` files across every initialized pool. Stale-lock signature + reclamation path live in [[lifecycle.md#crash-recovery]].

## Distribution

arm64 macOS only. Two tools end up on `$PATH`:

- `worktree-pool` — the Rust binary (the pool primitive). Built locally via `cargo build --release`; the install script symlinks `~/.local/bin/worktree-pool` → `target/release/worktree-pool`, so any subsequent `cargo build --release` updates the installed tool in place.
- `wt` — Bash wrapper for the common dev-session lifecycle. Project-agnostic; auto-resolves pool key from cwd (override with `--pool`). Symlinked from `bin/wt`.

`scripts/install.sh` (also `just install`) builds + symlinks both into `~/.local/bin/`.

```sh
git clone https://github.com/studio-boxcat/worktree-pool.git ~/Develop/worktree-pool
cd ~/Develop/worktree-pool && just install
echo 'export WORKTREE_ROOT="$HOME/.worktree-pool"' >> ~/.zshenv.local
worktree-pool doctor
```
