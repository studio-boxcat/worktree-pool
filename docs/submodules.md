# Submodules at acquire time

> **Related:** [[lifecycle.md]] (the acquire steps this backs), [[land-submodules.md]] (the same problem at land time), [[cli.md]]

## Mirror (mandatory when submodules exist)

A submodule's effective fetch URL is rewritten at acquire time to a **local
mirror** by `submodule_mirror_mode`:

| Mode | Effective URL | Resolves local-only pins? |
|------|---------------|---------------------------|
| `source-submodules` | `<base>/.git/modules/<composedName>` | **yes** — reads a working clone's own object store |
| `bare-mirror` | `<base>/<org>/<repo>.git` | only if the mirror is fresh |

Both need `submodule_mirror_base`. For a working-clone source you actively commit
in, use `source-submodules` with `base = source`: it resolves whatever the source
HEAD references, pushed or not. `base` may differ from `source` — e.g. a bare
source mirrored from its sibling working clone's `.git/modules`.

**There is deliberately no declared-URL fallback.** Without a mirror the only
remaining URL reaches the network, which fails mid-acquire with a cryptic `not
our ref` the moment a pin is local-only — a freshly-bumped-but-unpushed
submodule, the common dev case. So a missing mirror fails loud at two gates:

- **`init`** refuses a submodule-bearing source outright; no pool is created.
- **`acquire`** backstops pools predating that gate, or whose source gained
  submodules since. It bails *before* the idle→held flip, leaving the slot
  detached and reclaimable rather than held with a half-fetched tree.

URL writes are sequential under a per-source mutex
(`<source-gitdir>/worktree-pool-config.lock`) because they all target one
`<source>/.git/config`; parallel acquires sharing a source would otherwise fight
on git's lockfile. The fetches that follow are parallel per submodule.

## Filtering (`worktreePoolTag`)

Submodule taxonomy lives in the source repo's `.gitmodules`, so it is
version-controlled and propagates on the next checkout:

```ini
[submodule "Packages/com.unity.ide.rider"]
    path = Packages/com.unity.ide.rider
    url = git@github.com:org/com.unity.ide.rider.git
    worktreePoolTag = editor
```

`acquire --exclude-submodule-tags <t1,t2>` deinits and skips matching submodules
— a CI build can drop editor-only modules that a dev session wants. The key is
matched case-insensitively because git lowercases config keys on read.

Tags apply at **top level only**; a nested submodule initializes whenever its
parent is included.

`validate-gitmodules` reports misspelled `worktreePool*` keys in `unknown_keys`
and exits non-zero — git silently accepts a typo like `worktreePoolTags`, so
nothing else would catch it.
