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

- **`mutex::tests::is_held_false_when_unlocked` is flaky under full-suite load.**
  Fails roughly 1 run in 12 of `cargo test --release`, never in isolation or
  under `--lib` alone. The test drops a `FileLock` and asserts `is_held` is then
  false; `is_held` itself takes and releases the lock to probe, a race the
  function's own doc comment already calls out. Under parallel load the probe
  appears to observe a transient `WouldBlock` and report held. Either make the
  probe distinguish "free" from "contended", or drop the assertion's reliance on
  an inherently racy read.
