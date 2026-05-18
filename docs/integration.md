# Integration

> **Related:** [[../CLAUDE.md]], [[wt.md]] (hooks, land flow), [[cli.md]]

## Integration patterns

The minimal-friction integration is **no integration at all** — auto-resolution + `.wt-hooks.sh` covers the common cases. From inside the source repo or any slot, `wt go feature-x`, `wt land`, `wt ls`, `wt release feature-x` work without consumer wrappers. Project-specific extras live in `<source>/.wt-hooks.sh`; see [[wt.md#hooks-sourcewt-hookssh]].

Consumers only need a `just` recipe (or shell alias) when the wrapper adds *operator-facing* surface — independent verbs like `wt-meta`, `wt-dev-start` — not for pre-filling the pool key. Avoid the old pattern of a recipe per verb just to inject the key:

```bash
# OLD — redundant; auto-resolution makes this unnecessary.
wt-go name:    @wt go myapp {{quote(name)}}
wt-release name: @wt release myapp {{quote(name)}}
# … (delete; just type `wt go feature-x` directly)
```

Pool config (source path, mirror mode) lives in `<pool>/.meta/config.yaml` written once by `init`. Both pool config and `.wt-hooks.sh` are host-agnostic in practice — pool keys map to `$WORKTREE_ROOT/<key>/`, and the hooks file is version-controlled with the source, so the same setup runs on server and laptop.

---

## Multi-slot gotchas

Slots share `.git/` and `.git/modules/` with the source repo, but **not** the working tree. Things to know when running multiple slots concurrently:

- **Per-slot warmth lives inside the slot dir.** Build artifacts (Unity `Library/`, `Temp/`, `proj-*`, `node_modules/`, gradle caches) survive recycle because pool's `acquire` does `git reset --hard` only — never `git clean`. Across-platform flips inside a single slot rebuild platform-specific caches; don't symlink caches across slots.
- **Submodule git-dirs (`<source>/.git/modules/...`) are shared.** Concurrent submodule updates can race on ref locks (`index.lock` / `worktrees.lock`); git's own internal `O_EXCL` retry handles transient contention. A *crashed-git* leftover `index.lock` in a slot's per-worktree gitdir (distinct case — SIGKILL/panic mid-write) is swept on the next acquire/release; see [[lifecycle.md#crash-recovery]].
- **Shared docs (`TODO.md`, `CLAUDE.md`, `docs/`) are high-traffic.** Keep edits scoped, commit in their own commit, rebase early. A long-held dev session diverging on these is the usual source of conflicts.
- **Branch refs accumulate in the source repo.** `release` deletes the branch (local + remote best-effort), so steady state has zero buildup. Crashed acquires that bypass release leave orphans — operator can prune via `git for-each-ref refs/heads/ | xargs ...`.
- **LFS endpoint routing is consumer's responsibility.** Pool slots clone submodules from the source bare; if those submodules use LFS, smudging hits whatever `lfs.url` resolves to. Consumers using a remote LFS relay (e.g. EC2 reverse-proxy) should set a `[url] insteadOf` rewrite to a faster local endpoint where available. macmini-side example: `git config --global url.http://localhost:3690/.insteadOf https://relay.example/` (lives in `~/.gitconfig.local`, not the dotfiles repo, so it's machine-specific). Without the rewrite, cold acquires that hit LFS smudge incur per-object WAN round-trips. Pool tooling itself doesn't inspect or enforce this.

---

## What this tool does NOT do

Cuts that simplify the design:

- **No GC.** All cleanup is operator-explicit. Capacity-bound errors list the table; operator picks a slot to release.
- **No registry.** Pool key → path mapping is convention-based (`$WORKTREE_ROOT/<key>/`). No `~/.config/...`-tracked file.
- **No cross-host coordination.** Pools are host-local. Network-mounted shared pools are not supported (no `host`/`pid` liveness checks).
- **No reclaim on holder death.** A SIGKILL'd holder leaves a held marker; operator notices via `ls` and runs `release`. (Distinct from protocol-crash recovery, which IS automatic — see [[lifecycle.md#crash-recovery]].)
- **No `--fresh` / `--volatile` flags.** Caller wipes warmth itself if needed; release is the only "give back" verb.

If you need GC-like behavior, write a 5-line script: `worktree-pool ls` → filter → `release --name <X>` per match. Keeps the binary lean.

---

## Limits

- Branch refs accumulate in the source repo for SIGKILL'd builds and GC-style abandoned dev sessions (the latter intentional — work-recovery via `git branch | grep`). For high-volume CI, periodic `git for-each-ref --format='%(refname:short)' refs/heads/ | xargs -I X sh -c 'git merge-base --is-ancestor X origin/main && git branch -D X'` cleanup is the consumer's responsibility.
- Same-SHA exclusion is per-pool. Two pools sharing a source repo do not coordinate.
- `git status --porcelain` performance on huge worktrees (50k+ files) is the bottleneck for `ls --git-status`; plain `ls` is cheap (lock-mtime only).
