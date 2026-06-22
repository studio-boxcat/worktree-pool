# Integration

> **Related:** [[CLAUDE.md]], [[wt.md]] (hooks, land flow), [[cli.md]]

## Integration patterns

The minimal-friction integration is **no integration at all** — auto-resolution + `.wt-hooks.sh` covers the common cases. From inside the source repo or any slot, `wt go feature-x`, `wt land`, `wt ls`, `wt release feature-x` work without consumer wrappers. Project-specific extras live in `<source>/.wt-hooks.sh`; see [[wt.md#hooks-sourcewt-hookssh]].

Consumers only need a `just` recipe (or alias) when the wrapper adds *operator-facing* surface — independent verbs like `wt-meta`, `wt-dev-start` — not for pre-filling the pool key. Avoid the old pattern of a recipe per verb just to inject the key:

```bash
# OLD — redundant; auto-resolution makes this unnecessary.
wt-go name:    @wt go myapp {{quote(name)}}
wt-release name: @wt release myapp {{quote(name)}}
# … (delete; just type `wt go feature-x` directly)
```

Pool config (source path, mirror mode) lives in `config.yaml`, written once by `init`. Both it and `.wt-hooks.sh` are host-agnostic in practice — keys map to `$WORKTREE_ROOT/<key>/`, and the hooks file is version-controlled with the source, so the same setup runs on server and laptop.

---

## Multi-slot gotchas

Slots share `.git/` and `.git/modules/` with the source repo, but **not** the working tree. When running multiple slots concurrently:

- **Per-slot warmth lives inside the slot dir.** Build artifacts (Unity `Library/`, `Temp/`, `node_modules/`, gradle caches) survive recycle because the slot stays canonical (no rename) and acquire does `git reset --hard` only — never `git clean`. Stable abs paths preserve abs-path-keyed caches (Unity Bee, watchman, IDE indexes). Across-platform flips inside one slot rebuild platform-specific caches; don't symlink caches across slots.
- **Submodule git-dirs (`<source>/.git/modules/...`) are shared.** Concurrent updates can race on ref locks; git's own `O_EXCL` retry handles transient contention. A *crashed-git* leftover `index.lock` (distinct case) is removed by acquire when it recycles that slot — see [[lifecycle.md#crash-recovery]].
- **Shared docs (`TODO.md`, `CLAUDE.md`, `docs/`) are high-traffic.** Keep edits scoped, commit separately, rebase early. A long-held session diverging on these is the usual conflict source.
- **Branch refs accumulate in the source repo.** `release` deletes the branch (local + remote best-effort), so steady state is zero buildup. Crashed acquires that bypass release leave prunable orphans.
- **LFS endpoint routing is the consumer's responsibility.** Slots clone submodules from the source bare; if those use LFS, smudging hits whatever `lfs.url` resolves to. With a remote LFS relay, set a `[url] insteadOf` rewrite to a faster local endpoint — e.g. `git config --global url.http://localhost:3690/.insteadOf https://relay.example/` (lives in `~/.gitconfig.local`, machine-specific). Without it, cold acquires incur per-object WAN round-trips. Pool tooling doesn't inspect or enforce this.

---

## What this tool does NOT do

Cuts that simplify the design:

- **No GC.** All cleanup is operator-explicit. Capacity errors list the table; operator picks a slot to release.
- **No registry.** Pool key → path is convention (`$WORKTREE_ROOT/<key>/`). No tracked file.
- **No cross-host coordination.** Pools are host-local. Network-mounted shared pools aren't supported (no host/pid liveness checks).
- **No reclaim on holder death.** A SIGKILL'd holder leaves the slot held; operator notices via `ls` and runs `release`. (A crash *mid* acquire/release instead converges on its own — see [[lifecycle.md#crash-recovery]].)
- **No `--fresh` / `--volatile` flags.** Caller wipes warmth itself if needed; release is the only "give back" verb.

If you need GC-like behavior, write a 5-line script: `worktree-pool ls` → filter → `release <NAME>` per match.

Retry-aware CI callers branch on exit codes (see [[cli.md#exit-codes]]): **3 = contended** (retry), **4 = capacity** (release first), **5 = unique-sha conflict** (reuse holder's output). Everything else exits 1.

---

## Limits

- Branch refs accumulate for SIGKILL'd builds and abandoned dev sessions (the latter intentional — work recovery via `git branch | grep`). For high-volume CI, periodic `git for-each-ref --format='%(refname:short)' refs/heads/ | xargs -I X sh -c 'git merge-base --is-ancestor X origin/main && git branch -D X'` is the consumer's responsibility.
- Same-SHA exclusion is per-pool — two pools sharing a source don't coordinate.
- `git status --porcelain` on huge worktrees (50k+ files) is the bottleneck for `ls --git-status`; plain `ls` is cheap (gitdir HEAD read only).
