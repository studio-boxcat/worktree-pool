# TODO

Deferred work for worktree-pool. See `CLAUDE.md` for the design contract.

- **Pre-flight mirror resolves the pin, not just "a mirror is named".** The
  init gate + acquire backstop only assert that a submodule pool *has* a mirror
  mode — not that the configured mirror actually contains the pinned submodule
  SHA. A stale `bare-mirror`, a wrong `submodule_mirror_base`, or a `source-
  submodules` base that's been `gc`'d still fails deep inside `git submodule
  update` with git's own cryptic error — the exact experience the gate set out
  to remove, just relocated. Consider a cheap pre-flight (before materializing
  the slot, or before the held-flip) that checks each pinned submodule SHA is
  present in the resolved mirror, and fails loud there with a clear message.
