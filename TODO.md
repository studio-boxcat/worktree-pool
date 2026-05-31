# TODO

Deferred work for worktree-pool. See `CLAUDE.md` for the design contract.

- `bin/wt` land-step comments use an older numeric scheme (step 8/9/10) that no longer matches the authoritative Land-flow numbering in `[[docs/wt.md#land-flow]]` (steps 11/13). Renumber or drop the brittle numeric references. (code-comment drift; doc is correct)
- `benches/yaml.rs` — `#[cfg(test)] mod tests` has an unused `use super::*` (clippy warning). Trivial removal; left for a bench-targeted pass.
