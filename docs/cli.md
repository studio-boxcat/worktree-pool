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
# `$WORKTREE_ROOT/myapp/ios-0`) on stdout.
worktree-pool --pool myapp acquire --lease abc12345-ios --commit abc12345 --group ios

# Acquire at the pool's default_commit
worktree-pool --pool myapp acquire --lease feature-x --group ios

worktree-pool --pool myapp release --lease abc12345-ios

# Inspect. JSON out — see Output contract below.
worktree-pool --pool myapp ls | jq
worktree-pool --pool myapp ls --git-status          # adds a per-slot `git` object
worktree-pool --pool myapp inspect --lease abc12345-ios | jq
worktree-pool --pool myapp path --lease abc12345-ios   # slot path; exit 1 if not held
```

## CLI

```
worktree-pool --pool <key> init --source <repo> [--submodule-mirror-mode <source-submodules|bare-mirror> --submodule-mirror-base <p>] [--default-commit <ref>] --max-slots <n> [--groups <g1,g2>]
  # mirror flags are REQUIRED (together) when <repo> declares submodules — see [[lifecycle.md#submodule-mirror-mandatory-when-submodules-exist]]

worktree-pool --pool <key> acquire --lease <L> [--commit <commitish>] [--group <g>] [--exclude-submodule-tags <t1,t2>]

worktree-pool --pool <key> release --lease <L>
worktree-pool --pool <key> ls [--git-status]
worktree-pool --pool <key> inspect --lease <L>
worktree-pool --pool <key> path --lease <L>
worktree-pool --pool <key> unstick [--slot <id>]
worktree-pool --pool <key> validate-gitmodules

worktree-pool doctor
```

`--lease` is a flag rather than a positional because the lookup verbs also accept
a slot id, and one bare argument couldn't signal which you meant. Semantics:
[[lifecycle.md#identity-model]].

`acquire` fires the `wt_post_acquire` hook if the source ships `.wt-hooks.sh` —
fail-loud, so build pools get the same extension point as `wt go`. See
[[wt.md#hooks-sourcewt-hookssh]].

`unstick` is a read-only diagnostic: it reports whether each init mutex file is
currently locked by a live process. OS-managed flocks auto-release on process
death, so there's nothing to force-clear.

## Output contract

Stdout is machine-readable in every subcommand, in exactly one of three shapes:

| Shape | Verbs | Notes |
|-------|-------|-------|
| *(empty)* | `init`, `release` | Progress goes to stderr; there is no result to return. |
| One bare line | `acquire`, `path` | The canonical slot path. Consumed as `$(…)` — no parser in the hot loop. |
| One line of compact JSON | `ls`, `inspect`, `unstick`, `validate-gitmodules`, `doctor` | `\| jq` to read. |

There is **no `--json` flag**: a verb that reports structure always reports JSON,
so callers never branch on format and no human-readable table can drift from the
machine-readable one. Rendering is the consumer's job — `wt` does it for
operators (see [[wt.md]]).

Conventions:

- **Everything else is stderr.** Logs, warnings, hook output, and the `error: …`
  line never touch stdout.
- **Absent data is `null`**, never a placeholder like `"-"`. An idle slot has
  `"lease": null`; `"git"` is absent entirely unless `ls --git-status` was passed.
- **A report and a failure are independent.** `doctor` and `validate-gitmodules`
  print their full report *and* exit non-zero when it contains problems — the
  payload is the point, so it survives the failure. `path` inverts this: exit 1
  with empty stdout *and* empty stderr, so `if worktree-pool … path …; then` reads
  cleanly.

`src/output.rs` defines the three shapes and `main` is the only writer, so a
subcommand cannot print off-contract; the field-level schema of each report lives
in the verb's own module and is locked by `tests/lifecycle.rs`.

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
| 6    | LeaseHeld   | The requested lease is already held                              | Reuse / release holder |

5 is retired and stays unassigned: callers branch on these, so new conditions get
new codes and existing ones never shift. The contract is locked by
`tests/lifecycle.rs::acquire_capacity_exhaustion` and
`acquire_refuses_a_lease_already_held`.

`doctor` is host-level (no `--pool`) and read-only. Checks: arch, `git --version`,
`$WORKTREE_ROOT` + pool count, binary quarantine xattr, and per-pool config +
source-path validation. Its report is `SectionResult[]` — boxcat-ts-core's doctor
wire format, which boxcat-devenv's doctor parses whole and merges into its own.

## Distribution

arm64 macOS only. Two tools end up on `$PATH`:

- `worktree-pool` — the Rust binary (the pool primitive).
- `wt` — Bash wrapper for the common dev-session lifecycle; auto-resolves pool
  key from cwd (override with `--pool`). Symlinked from `bin/wt`.

`just install` installs the binary into `~/.cargo/bin` (`cargo install --path`, the repo's
`target/` as build cache); `wt` is linked into `~/.local/bin` by boxcat-devenv's bootstrap, as
this repo's manifest declares.

```sh
git clone https://github.com/studio-boxcat/worktree-pool.git ~/Develop/worktree-pool
cd ~/Develop/worktree-pool && just install
worktree-pool doctor
```
