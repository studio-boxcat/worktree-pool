//! Lifecycle hook dispatch for the verbs worktree-pool *core* owns.
//!
//! A source repo may ship `.wt-hooks.sh` defining bash functions that extend
//! lifecycle verbs (see CLAUDE.md §Hooks). The `wt` dev wrapper fires its own
//! wrapper-only hooks (go / cleanup / land); core fires the hooks for verbs
//! that are core commands — today just `acquire` (`wt_post_acquire`), so the
//! hook runs for direct `worktree-pool acquire` (CI / build pools) as well as
//! a fresh `wt go`. See [[docs/wt.md#hooks-sourcewt-hookssh]] for the contract.
//!
//! The file is read from the **acquired slot's checkout** — not the source —
//! because the source may be bare (CLAUDE.md: source can be bare or working
//! clone), which has no working-tree `.wt-hooks.sh`. The slot is always a
//! checkout, so a committed hook is present there for bare and working sources
//! alike, and it reflects the exact acquired commit's hook. (`wt`, by
//! contrast, sources from the source worktree — it fires *before* a slot
//! exists, e.g. `wt_pre_go`.)
//!
//! Each hook runs **in the slot** with the `WT_KEY / WT_LEASE / WT_PATH /
//! WT_FRESH` env contract set. A non-zero hook is a hard error (fail-loud):
//! the caller decides whether to gate on it.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

pub const HOOKS_FILE: &str = ".wt-hooks.sh";

/// Fire `hook_name` from `<slot_path>/.wt-hooks.sh` if the file exists and the
/// function is defined; a no-op otherwise. Runs in `slot_path` with the
/// `WT_*` contract exported. `hook_name` is always a crate-internal literal
/// (never user input), so its interpolation into the bash snippet is safe.
pub fn fire(
    hook_name: &str,
    slot_path: &Path,
    pool_key: &str,
    name: &str,
    fresh: bool,
) -> Result<()> {
    let hooks_file = slot_path.join(HOOKS_FILE);
    if !hooks_file.exists() {
        return Ok(());
    }
    // Reserve stdout for acquire's path line: `exec 1>&2` routes the hook's
    // stdout to stderr so a chatty hook can't pollute what build-pool callers
    // parse (acquire prints the slot path on stdout *after* this returns).
    //
    // `bash -n` rejects a *syntactically* broken hooks file loud — otherwise
    // `source` aborts mid-file, the function is never defined, the `declare -F`
    // guard skips it, and the acquire would succeed with a gate hook that never
    // ran (silent bypass).
    //
    // Then source the file *outside* `set -e` — a benign non-zero top-level
    // statement (e.g. a `command -v foo` guard) must not abort when the target
    // hook itself is fine; matches `wt`'s `source_hooks`. The hook *body* runs
    // *under* `set -e`: wt_post_acquire is a fail-loud GATE (it may reject the
    // acquire), unlike `wt`'s best-effort wrapper hooks — a deliberate asymmetry.
    //
    // `hook_name` is a crate-internal literal (safe to interpolate); the slot
    // path is passed via the `$WT_HOOKS_FILE` env var, never interpolated, since
    // it is not a literal.
    let script = format!(
        "exec 1>&2; \
         if ! bash -n \"$WT_HOOKS_FILE\"; then \
           echo \"worktree-pool: {hook_name}: .wt-hooks.sh has a syntax error\"; exit 1; \
         fi; \
         source \"$WT_HOOKS_FILE\"; set -e; \
         if declare -F {hook_name} >/dev/null 2>&1; then {hook_name}; fi"
    );
    let status = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(slot_path)
        .env("WT_HOOKS_FILE", &hooks_file)
        .env("WT_KEY", pool_key)
        .env("WT_LEASE", name)
        .env("WT_PATH", slot_path)
        .env("WT_FRESH", if fresh { "1" } else { "0" })
        .status()
        .with_context(|| format!("spawning bash for hook {hook_name}"))?;
    if !status.success() {
        bail!(
            "hook {hook_name} failed ({status}); see {}",
            hooks_file.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn missing_file_is_noop() {
        let tmp = TempDir::new().unwrap();
        assert!(fire("wt_post_acquire", tmp.path(), "k", "n", true).is_ok());
    }

    #[test]
    fn undefined_function_is_noop() {
        let tmp = TempDir::new().unwrap();
        // A different hook is defined; ours is absent → no-op (and would error if run).
        fs::write(tmp.path().join(HOOKS_FILE), "wt_pre_go() { exit 7; }\n").unwrap();
        assert!(fire("wt_post_acquire", tmp.path(), "k", "n", true).is_ok());
    }

    #[test]
    fn top_level_nonzero_does_not_abort() {
        // A benign non-zero top-level statement (here `false`) must not fail the
        // hook when the target function is undefined — the file is sourced
        // outside `set -e`. Regression guard for the source-under-set-e bug.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(HOOKS_FILE), "false\nwt_pre_go() { :; }\n").unwrap();
        assert!(fire("wt_post_acquire", tmp.path(), "k", "n", true).is_ok());
    }

    #[test]
    fn runs_in_slot_with_env_contract() {
        let tmp = TempDir::new().unwrap();
        let slot = tmp.path().join("slot");
        fs::create_dir(&slot).unwrap();
        // The hook file lives in the slot checkout (core reads it from there).
        // Relative `out` proves cwd == slot; the body captures the full WT_*
        // contract including WT_PATH (the export the path-isolation relies on).
        fs::write(
            slot.join(HOOKS_FILE),
            "wt_post_acquire() { printf '%s|%s|%s|%s' \"$WT_KEY\" \"$WT_LEASE\" \"$WT_FRESH\" \"$WT_PATH\" > out; }\n",
        )
        .unwrap();
        fire("wt_post_acquire", &slot, "mykey", "myname", true).unwrap();
        assert_eq!(
            fs::read_to_string(slot.join("out")).unwrap(),
            format!("mykey|myname|1|{}", slot.display())
        );
    }

    #[test]
    fn malformed_file_is_error() {
        // A syntax error means `source` aborts and the function is never defined.
        // Without the `bash -n` guard this would be a silent no-op (Ok), bypassing
        // a gate hook — so it must surface as a hard error (fail-loud contract).
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(HOOKS_FILE),
            "wt_post_acquire() { echo (broken; }\n",
        )
        .unwrap();
        let err = fire("wt_post_acquire", tmp.path(), "k", "n", true).unwrap_err();
        assert!(format!("{err:#}").contains("wt_post_acquire"));
    }

    #[test]
    fn failing_hook_is_error() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(HOOKS_FILE),
            "wt_post_acquire() { return 3; }\n",
        )
        .unwrap();
        let err = fire("wt_post_acquire", tmp.path(), "k", "n", false).unwrap_err();
        assert!(format!("{err:#}").contains("wt_post_acquire"));
    }
}
