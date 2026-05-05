//! `git` shell-out helpers. We profiled both `gix` and `git2` and shell-out won
//! on binary size + startup (see `resolve_full_sha` doc + da8b429); the ~10ms
//! per-spawn savings were dwarfed by submodule clones anyway. We also don't use
//! `git worktree move` (refuses on slots with submodules) — see `worktree_rename`
//! for the `fs::rename` + `git worktree repair` + admin rewrite flow that replaces it.
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `git <args>` with `-C cwd`, capture stdout+stderr. Returns the trimmed stdout
/// on success; on non-zero exit, returns an error including the stderr.
pub fn run(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = spawn(cwd, &[], args)?;
    check(&out, cwd, args)
}

/// Like `run`, but with extra `-c key=value` overrides before the subcommand.
/// Order: `-C cwd -c k=v ... <args>`.
pub fn run_with_config(cwd: &Path, overrides: &[(&str, &str)], args: &[&str]) -> Result<String> {
    let out = spawn(cwd, overrides, args)?;
    check(&out, cwd, args)
}

/// Like `run` but doesn't error on non-zero exit. Returns (success, stdout, stderr).
pub fn run_lenient(cwd: &Path, args: &[&str]) -> Result<(bool, String, String)> {
    let out = spawn(cwd, &[], args)?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

fn spawn(cwd: &Path, overrides: &[(&str, &str)], args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(cwd);
    for (k, v) in overrides {
        cmd.arg("-c").arg(format!("{k}={v}"));
    }
    cmd.args(args);
    cmd.output()
        .with_context(|| format!("spawning git -C {} {}", cwd.display(), args.join(" ")))
}

fn check(out: &Output, cwd: &Path, args: &[&str]) -> Result<String> {
    if !out.status.success() {
        bail!(
            "git -C {} {} failed (exit {}): {}",
            cwd.display(),
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git -C dir config --file <file> --list`. Returns the raw key=value lines.
pub fn config_file_list(cwd: &Path, file: &Path) -> Result<String> {
    run(
        cwd,
        &[
            "config",
            "--file",
            &file.display().to_string(),
            "--list",
        ],
    )
}

/// Resolve a commit-ish to its full 40-char SHA against `source`.
///
/// We tried both `gix` and `git2` (vendored libgit2). Both shaved ~10 ms off `acquire`
/// (1 spawn eliminated) but penalized `ls` and `release` startup. See da8b429 commit
/// message for the full 3-way profile matrix.
pub fn resolve_full_sha(source: &Path, commitish: &str) -> Result<String> {
    let arg = format!("{commitish}^{{commit}}");
    run(source, &["rev-parse", "--verify", &arg])
        .with_context(|| format!("resolving '{commitish}' in {}", source.display()))
}

/// Returns the absolute path of the worktree's gitdir
/// (e.g. `<source>/.git/worktrees/<id>`).
///
/// In a non-main worktree, `<slot>/.git` is a one-line gitlink file: `gitdir: <abs-path>`.
/// We parse it directly to avoid spawning `git rev-parse --git-dir` (~40 ms per call;
/// hot in `ls` and same-SHA scan). Falls through to the spawn-based path if the file
/// shape is unexpected (e.g. someone ran the tool against a main worktree by mistake).
pub fn worktree_gitdir(slot: &Path) -> Result<PathBuf> {
    let gitlink = slot.join(".git");
    if let Ok(text) = std::fs::read_to_string(&gitlink)
        && let Some(rest) = text.strip_prefix("gitdir: ")
    {
        let path = PathBuf::from(rest.trim());
        return Ok(if path.is_absolute() {
            path
        } else {
            slot.join(path)
        });
    }
    let gd = run(slot, &["rev-parse", "--git-dir"])?;
    let p = PathBuf::from(&gd);
    if p.is_absolute() {
        Ok(p)
    } else {
        Ok(slot.join(p))
    }
}

/// `git -C source worktree add --detach <slot> <commit>`.
pub fn worktree_add(source: &Path, slot: &Path, commit: &str) -> Result<()> {
    run(
        source,
        &[
            "worktree",
            "add",
            "--detach",
            &slot.display().to_string(),
            commit,
        ],
    )
    .map(drop)
}

/// Rename a worktree directory and repair every back-pointer that names the old path.
///
/// We can't use `git worktree move` because it refuses on worktrees that contain
/// initialized submodules ("fatal: working trees containing submodules cannot be
/// moved or removed") — and every recycled slot in this pool has submodules. So
/// we do the rename ourselves with `fs::rename` (atomic on the same filesystem,
/// indifferent to submodules), then fix back-pointers in two places:
///   1. `<source>/.git/worktrees/<id>/gitdir` — handled by `git worktree repair`.
///   2. `<source>/.git/worktrees/<id>/modules/**/config` — each submodule admin's
///      `core.worktree` is a relative path that names the OLD slot dir; `git worktree
///      repair` does not recurse, so we rewrite these ourselves by replacing the
///      slot-name PATH SEGMENT (anchored to the pool-key parent) — see
///      `rewrite_submodule_worktrees` for why naive substring substitution is unsafe.
pub fn worktree_rename(source: &Path, from: &Path, to: &Path) -> Result<()> {
    // Both args must be siblings under the same pool root — pool-key derivation
    // assumes it. Pure invariant-check; both call sites pass `pool_root.join(...)`.
    debug_assert_eq!(
        from.parent(),
        to.parent(),
        "worktree_rename: from and to must share a parent (pool root)"
    );
    // Pre-flight: if `to` already exists, refuse rather than risk clobbering a
    // stale dir from a prior crash. `fs::rename` on macOS can replace an existing
    // empty/dir target; this turns that footgun into a loud error.
    if to.try_exists().unwrap_or(false) {
        bail!(
            "worktree_rename: target {to_disp} already exists — refusing to clobber. \
             Likely leftover state from a prior crashed acquire/release. Recover with:\n  \
               git -C {src_disp} worktree remove --force {to_disp}\n  \
               rm -rf {to_disp}\n\
             then retry.",
            to_disp = to.display(),
            src_disp = source.display()
        );
    }
    std::fs::rename(from, to)
        .with_context(|| format!("renaming {} → {}", from.display(), to.display()))?;
    run(
        source,
        &["worktree", "repair", &to.display().to_string()],
    )?;
    let from_name = from
        .file_name()
        .and_then(|s| s.to_str())
        .context("from path has no final component")?;
    let to_name = to
        .file_name()
        .and_then(|s| s.to_str())
        .context("to path has no final component")?;
    if from_name == to_name {
        return Ok(());
    }
    // The pool-key segment (the parent dir of the slot) anchors the rewrite —
    // only the slot-name segment that immediately follows it gets rewritten,
    // never another segment that happens to share the slot's name.
    //
    // Symlinked pool roots: if `~/.worktree-pool/<key>` is a symlink, this
    // derivation uses the symlink basename. The README's documented form
    // (`ln -s /Volumes/big/<key> ~/.worktree-pool/<key>`) keeps both names
    // identical, so the anchor still matches `core.worktree`'s segments. If
    // the target dir name DIFFERS from the symlink basename, segment-anchored
    // rewrite would silently no-op — see TODO.md and README §Layout for the
    // documented constraint.
    let pool_key = to
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .context("to path has no parent dir name (pool-key)")?;
    let _ = from_name;
    let admin = worktree_gitdir(to)?;
    let modules_root = admin.join("modules");
    if modules_root.exists() {
        rewrite_submodule_worktrees(&modules_root, pool_key, to_name)?;
    }
    Ok(())
}

/// Walk every `<modules_root>/**/config` and rewrite the slot-name path segment in
/// each `core.worktree` value, anchored to the pool-key segment that precedes it.
///
/// Why anchored to pool-key only (not from-name)? Two reasons:
///   1. `core.worktree` has shape `../../<...>/<pool-key>/<slot-name>/<sub-path>`,
///      and the slot-name appears EXACTLY ONCE between `<pool-key>` and `<sub-path>`.
///      A naive `value.replace("/old/", "/new/")` corrupts paths whenever a slot
///      name happens to match a sub-path directory component (e.g. a Unity slot
///      named `Packages` collides with `Packages/com.foo`).
///   2. Idempotent self-heal. If a previous `worktree_rename` failed mid-walk and
///      left mixed state — some configs at `from-name`, others stuck at an even
///      earlier name — anchoring on pool-key alone (not requiring the segment to
///      match `from-name`) ensures every config still pointing through the slot
///      tree gets unconditionally normalized to `to-name`. This closes the silent-
///      corruption path described in TODO.md (where partial prior-rewrite leftovers
///      would be invisibly skipped on a subsequent rename).
fn rewrite_submodule_worktrees(
    modules_root: &Path,
    pool_key: &str,
    to_name: &str,
) -> Result<()> {
    let mut stack = vec![modules_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() && entry.file_name() == "config" {
                rewrite_config_worktree(&path, pool_key, to_name)?;
            }
        }
    }
    Ok(())
}

fn rewrite_config_worktree(
    config: &Path,
    pool_key: &str,
    to_name: &str,
) -> Result<()> {
    let text = std::fs::read_to_string(config)
        .with_context(|| format!("reading {}", config.display()))?;
    let mut changed = false;
    let new: String = text
        .lines()
        .map(|line| {
            // Match `\tworktree = ...` (git config indents body lines with a tab).
            let trimmed = line.trim_start();
            if let Some(value) = trimmed.strip_prefix("worktree = ")
                && let Some(new_value) = rewrite_slot_segment(value, pool_key, to_name)
            {
                changed = true;
                let prefix_len = line.len() - trimmed.len();
                format!("{}worktree = {}", &line[..prefix_len], new_value)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !changed {
        return Ok(());
    }
    let mut out = new;
    if text.ends_with('\n') {
        out.push('\n');
    }
    crate::atomic::write(config, &out)
        .with_context(|| format!("writing {}", config.display()))?;
    Ok(())
}

/// Rewrite the slot-name segment in a `core.worktree` relative path: find the
/// segment immediately following `<pool_key>` and replace it with `<to_name>`.
/// Returns `None` if the value doesn't reference the slot tree (no `<pool_key>`
/// segment) or already has the correct slot name (caller skips the write).
///
/// Idempotent and self-healing: doesn't require the current value to match any
/// particular `from_name`. A leftover stale segment from a partial prior rewrite
/// gets normalized on the next rename. See `rewrite_submodule_worktrees` doc.
fn rewrite_slot_segment(value: &str, pool_key: &str, to_name: &str) -> Option<String> {
    let mut segments: Vec<&str> = value.split('/').collect();
    for i in 1..segments.len() {
        if segments[i - 1] == pool_key {
            if segments[i] == to_name {
                return None; // already correct, no write needed
            }
            segments[i] = to_name;
            return Some(segments.join("/"));
        }
    }
    None
}

/// `git -C source worktree remove --force <slot>`. Best-effort.
pub fn worktree_remove(source: &Path, slot: &Path) -> Result<(bool, String, String)> {
    run_lenient(
        source,
        &[
            "worktree",
            "remove",
            "--force",
            &slot.display().to_string(),
        ],
    )
}

pub fn reset_hard(slot: &Path, commit: &str) -> Result<()> {
    run(slot, &["reset", "--hard", commit]).map(drop)
}

/// Force-create `<name>` at HEAD and attach HEAD to it. Tree-touchless.
///
/// Why not `git checkout -B <name>`? Even though HEAD is already at the right
/// tree (we just `worktree add`'d it), `checkout` walks the entire index and
/// queries every file through registered filters (`filter.lfs.process`,
/// `filter-process` stdio per file). For meow-tower's 45K files that's ~600ms
/// of pure no-op filter pings. `update-ref` + `symbolic-ref` does the actual
/// state change (branch ref + HEAD attach) in ~12ms without touching files.
pub fn checkout_force_branch(slot: &Path, name: &str) -> Result<()> {
    let ref_path = format!("refs/heads/{name}");
    run(slot, &["update-ref", &ref_path, "HEAD"])?;
    // `-m` writes a HEAD reflog entry — without it, `git reflog HEAD` won't show
    // the branch attach. Cheap; aids debugging when poking at slot state by hand.
    run(
        slot,
        &["symbolic-ref", "-m", "worktree-pool acquire", "HEAD", &ref_path],
    )
    .map(drop)
}

/// Detach HEAD (so we can delete the current branch).
pub fn checkout_detach(slot: &Path) -> Result<(bool, String, String)> {
    run_lenient(slot, &["checkout", "--detach"])
}

/// Best-effort branch delete.
pub fn branch_delete(slot: &Path, name: &str) -> Result<(bool, String, String)> {
    run_lenient(slot, &["branch", "-D", name])
}

/// Best-effort remote branch delete.
pub fn push_delete(slot: &Path, remote: &str, name: &str) -> Result<(bool, String, String)> {
    run_lenient(slot, &["push", remote, "--delete", name])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_slot_segment_basic() {
        let v = "../../../../../../../../../../.worktree-pool/meow-tower/old/Packages/com.foo";
        let out = rewrite_slot_segment(v, "meow-tower", "new").unwrap();
        assert_eq!(out, "../../../../../../../../../../.worktree-pool/meow-tower/new/Packages/com.foo");
    }

    /// Regression: slot named `Packages` collides with a sub-path component.
    /// Naive `value.replace("/Packages/", "/new/")` would rewrite BOTH segments
    /// (the slot AND the submodule's `Packages/com.foo` parent dir). The anchored
    /// form rewrites only the segment immediately after the pool-key.
    #[test]
    fn rewrite_slot_segment_does_not_corrupt_when_subpath_repeats_slotname() {
        let v = "../../../../../../../../../../.worktree-pool/meow-tower/Packages/Packages/com.foo";
        let out = rewrite_slot_segment(v, "meow-tower", "ios-0").unwrap();
        // Only the slot segment (after `meow-tower/`) is rewritten; the inner
        // `Packages/com.foo` is preserved.
        assert_eq!(out, "../../../../../../../../../../.worktree-pool/meow-tower/ios-0/Packages/com.foo");
    }

    #[test]
    fn rewrite_slot_segment_does_not_corrupt_when_slotname_equals_pool_key() {
        // Pathological case: someone names the slot the same as the pool key.
        // The pool-key anchor still picks the right segment (the one after the pool-key).
        let v = "../../.worktree-pool/meow-tower/meow-tower/sub";
        let out = rewrite_slot_segment(v, "meow-tower", "ios-0").unwrap();
        assert_eq!(out, "../../.worktree-pool/meow-tower/ios-0/sub");
    }

    #[test]
    fn rewrite_slot_segment_returns_none_when_path_does_not_match() {
        // Value doesn't reference the renamed slot at all.
        let out = rewrite_slot_segment("../../somewhere/else", "meow-tower", "new");
        assert!(out.is_none());
    }

    #[test]
    fn rewrite_slot_segment_returns_none_when_already_correct() {
        // Idempotent: re-running on already-rewritten value is a no-op.
        let out = rewrite_slot_segment(
            "../../.worktree-pool/meow-tower/ios-0/sub",
            "meow-tower",
            "ios-0",
        );
        assert!(out.is_none());
    }

    /// Self-heal regression: simulates the silent-corruption scenario from TODO.md
    /// where a previous rewrite failed mid-flight, leaving `core.worktree` stuck at
    /// some intermediate slot name (`canonical1`) — neither the rename's source
    /// (`userA`) nor the rename's target (`canonical2`). The new anchor-on-pool-key
    /// semantics rewrite the segment regardless of what it currently is, normalizing
    /// to `to_name` and closing the silent-corruption path.
    #[test]
    fn rewrite_slot_segment_self_heals_stale_value() {
        let v = "../../.worktree-pool/meow-tower/canonical1/Packages/com.foo";
        // Rename target is canonical2; current value still says canonical1 (stale).
        let out = rewrite_slot_segment(v, "meow-tower", "canonical2").unwrap();
        assert_eq!(out, "../../.worktree-pool/meow-tower/canonical2/Packages/com.foo");
    }

    #[test]
    fn rewrite_slot_segment_handles_absolute_paths() {
        let v = "/Users/x/.worktree-pool/meow-tower/old-name/Packages/com.foo";
        let out = rewrite_slot_segment(v, "meow-tower", "new-name").unwrap();
        assert_eq!(out, "/Users/x/.worktree-pool/meow-tower/new-name/Packages/com.foo");
    }
}
