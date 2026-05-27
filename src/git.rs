//! `git` shell-out helpers. We profiled both `gix` and `git2` and shell-out won
//! on binary size + startup (see `resolve_full_sha` doc + da8b429); the ~10ms
//! per-spawn savings were dwarfed by submodule clones anyway.
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
    git_dir_abs(slot)
}

/// Resolve `source`'s own gitdir (the source repo's git directory; same as
/// source for a bare repo, `<source>/.git` for a working clone). Used to
/// anchor the per-source mutex serializing top-level submodule config writes.
pub fn source_gitdir(source: &Path) -> Result<PathBuf> {
    git_dir_abs(source)
}

/// `git -C dir rev-parse --git-dir`, absolutized against `dir` when relative.
fn git_dir_abs(dir: &Path) -> Result<PathBuf> {
    let p = PathBuf::from(run(dir, &["rev-parse", "--git-dir"])?);
    Ok(if p.is_absolute() { p } else { dir.join(p) })
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

/// Detach HEAD (so we can delete the current branch) via plumbing — rev-parse
/// HEAD + `update-ref --no-deref HEAD <sha>`. Pure ref manipulation: no index
/// touch, no working-tree walk, so no contention with a crashed-git
/// `<gitdir>/index.lock` (which previously blocked `git checkout --detach`
/// and left dangling branch refs).
pub fn detach_head(slot: &Path) -> Result<(bool, String, String)> {
    let (ok, stdout, stderr) = run_lenient(slot, &["rev-parse", "HEAD"])?;
    if !ok {
        return Ok((false, stdout, stderr));
    }
    let sha = stdout.trim();
    if sha.is_empty() {
        return Ok((false, String::new(), "rev-parse HEAD returned empty".into()));
    }
    run_lenient(slot, &["update-ref", "--no-deref", "HEAD", sha])
}

/// Best-effort branch delete.
pub fn branch_delete(slot: &Path, name: &str) -> Result<(bool, String, String)> {
    run_lenient(slot, &["branch", "-D", name])
}

/// Best-effort remote branch delete.
pub fn push_delete(slot: &Path, remote: &str, name: &str) -> Result<(bool, String, String)> {
    run_lenient(slot, &["push", remote, "--delete", name])
}

/// `git symbolic-ref --short HEAD`. Returns `Some(branch)` when HEAD is attached;
/// `None` when detached or unreadable. Source of truth for a slot's user-facing
/// name: acquire creates branch `<name>`; release looks slots up by this.
pub fn current_branch(slot: &Path) -> Option<String> {
    let (ok, stdout, _stderr) = run_lenient(slot, &["symbolic-ref", "--short", "--quiet", "HEAD"]).ok()?;
    if !ok || stdout.is_empty() {
        return None;
    }
    Some(stdout)
}
