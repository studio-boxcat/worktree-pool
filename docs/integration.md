# Integration

> **Related:** [[CLAUDE.md]], [[wt.md]] (hooks, land flow), [[cli.md]] (output contract, exit codes)

## Integration patterns

The minimal-friction integration is **no integration at all** — pool-key auto-resolution plus `.wt-hooks.sh` covers the common cases. From inside the source repo or any slot, `wt go feature-x`, `wt land`, `wt ls`, `wt release feature-x` work with no consumer wrapper and no pool key. Project-specific extras belong in `<source>/.wt-hooks.sh`; see [[wt.md#hooks-sourcewt-hookssh]].

Reach for a `just` recipe or alias only when it adds *operator-facing* surface — an independent verb like `wt-dev-start`. A recipe that exists purely to inject the pool key is redundant.

Pool config (source path, mirror mode) lives in `config.yaml`, written once by `init`. Both it and `.wt-hooks.sh` are host-agnostic in practice — keys map to `$WORKTREE_ROOT/<key>/` and the hooks file is version-controlled with the source, so one setup runs on server and laptop alike.

Retry-aware CI callers branch on [[cli.md#exit-codes]] rather than parsing stderr.

---

## Multi-slot gotchas

Slots share `.git/` and `.git/modules/` with the source repo, but **not** the working tree. When running several concurrently:

- **Per-slot warmth lives inside the slot dir** and survives recycle (see [[lifecycle.md#identity-model]]). Flipping platforms *within* one slot rebuilds that platform's caches; don't symlink caches across slots to dodge it.
- **Submodule git-dirs (`<source>/.git/modules/...`) are shared.** Concurrent updates can race on ref locks; git's own `O_EXCL` retry absorbs transient contention. A crashed-git leftover `index.lock` is a distinct case — see [[lifecycle.md#crash-recovery]].
- **Shared docs (`TODO.md`, `CLAUDE.md`, `docs/`) are high-traffic.** Keep edits scoped, commit separately, rebase early. A long-held session diverging on these is the usual conflict source.
- **LFS endpoint routing is the consumer's responsibility.** Slots clone submodules from the source bare; if those use LFS, smudging hits whatever `lfs.url` resolves to. With a remote relay, rewrite it to a local endpoint — `git config --global url.http://localhost:3690/.insteadOf https://relay.example/` in `~/.gitconfig.local`. Without it, cold acquires pay per-object WAN round-trips. Pool tooling neither inspects nor enforces this.

---

## Scope boundaries

Cuts that simplify the design, and the limits they imply:

- **No GC.** All cleanup is operator-explicit; capacity errors list the held slots to pick from.
- **No registry.** Pool key → path is convention (`$WORKTREE_ROOT/<key>/`), not a tracked file.
- **No cross-host coordination.** Pools are host-local. Network-mounted shared pools aren't supported — there are no host/pid liveness checks.
- **No reclaim on holder death.** A SIGKILL'd holder leaves the slot held; the operator spots it via `ls` and releases. (A crash *mid* acquire/release converges on its own — [[lifecycle.md#crash-recovery]].)
- **No `--fresh` / `--volatile` flags.** `release` is the only "give back" verb; a caller that wants a cold slot wipes it itself.
- **No cross-pool coordination.** Duplicate-work refusal is per-pool and lease-keyed, so two pools sharing a source don't see each other ([[lifecycle.md#identity-model]]).

**Branch refs accumulate** when a holder dies before `release` — and, deliberately, for abandoned dev sessions, since the branch is how that work is recovered (`git branch | grep`). Steady state is otherwise zero: `release` deletes the branch. High-volume CI can prune periodically:

```sh
git for-each-ref --format='%(refname:short)' refs/heads/ \
  | xargs -I X sh -c 'git merge-base --is-ancestor X origin/main && git branch -D X'
```

GC-like reaping is the same shape — `ls` reports JSON ([[cli.md#output-contract]]), so the filter is a `jq` select:

```sh
worktree-pool --pool myapp ls --git-status \
  | jq -r '.[] | select(.state == "held" and .git.ahead == 0) | .lease' \
  | xargs -I L worktree-pool --pool myapp release --lease L
```

`git status --porcelain` on huge worktrees (50k+ files) is the bottleneck for `ls --git-status`; plain `ls` is cheap — a gitdir HEAD read per slot, no subprocess.
