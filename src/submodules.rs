//! Submodule update with URL rewriting (bare-mirror / git-modules) and tag-based filtering.
//!
//! Reads `<slot>/.gitmodules` to enumerate submodules + their `worktreePoolTag` values.
//! For each submodule:
//!  - skip if its tag is in `--exclude-submodule-tags`, AND deinit if currently inited;
//!  - otherwise: register `submodule.<name>.url` directly via `git config` (skips
//!    the submodule--helper overhead of `git submodule init`), then run a parallel
//!    `git submodule update <path>` per submodule via `std::thread::scope`.
//!
//! Recurses into nested `.gitmodules`. `editorOnly`-style filtering applies only to top-level
//! submodules (the convention is Unity-Package-specific even though this tool is generic).
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::cli::SubmoduleMirrorMode;
use crate::config::PoolConfig;
use crate::{fs_paths, git, mutex};

#[derive(Debug)]
struct SubmoduleEntry {
    name: String,
    path: String,
    url: String,
    tags: Vec<String>,
}

fn parse_gitmodules(slot_dir: &Path) -> Result<Vec<SubmoduleEntry>> {
    let gitmodules_path = slot_dir.join(".gitmodules");
    if !gitmodules_path.exists() {
        return Ok(Vec::new());
    }
    let raw = git::config_file_list(slot_dir, &gitmodules_path)
        .with_context(|| format!("reading {}", gitmodules_path.display()))?;
    parse_gitmodules_text(&raw)
}

/// Yield `(submodule_name, key, value)` for each `submodule.<name>.<key>=<value>` line.
/// git lowercases keys on read regardless of source casing — so `worktreePoolTag`,
/// `worktreepooltag`, and `WORKTREEPOOLTAG` all read as `worktreepooltag`.
pub fn iter_keys(raw: &str) -> impl Iterator<Item = (&str, &str, &str)> {
    raw.lines().filter(|l| !l.is_empty()).filter_map(|line| {
        let rest = line.strip_prefix("submodule.")?;
        let (sub_and_key, value) = rest.split_once('=')?;
        let (name, key) = sub_and_key.rsplit_once('.')?;
        Some((name, key, value))
    })
}

#[derive(Default)]
struct ParseBuf {
    path: Option<String>,
    url: Option<String>,
    tags: Vec<String>,
}

fn parse_gitmodules_text(raw: &str) -> Result<Vec<SubmoduleEntry>> {
    let mut by_name: HashMap<String, ParseBuf> = HashMap::new();
    for (sub_name, key, value) in iter_keys(raw) {
        let buf = by_name.entry(sub_name.to_string()).or_default();
        match key {
            "path" => buf.path = Some(value.to_string()),
            "url" => buf.url = Some(value.to_string()),
            "worktreepooltag" => buf.tags.push(value.to_string()),
            _ => {}
        }
    }
    let mut out: Vec<SubmoduleEntry> = by_name
        .into_iter()
        .filter_map(|(name, buf)| {
            Some(SubmoduleEntry {
                name,
                path: buf.path?,
                url: buf.url?,
                tags: buf.tags,
            })
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path)); // deterministic output
    Ok(out)
}

/// Compose a submodule's effective URL based on the pool's mirror config.
/// `name_scope` is the prefix under `<source>/.git/modules/` for nested submodules
/// (`""` at top level, `<parent_composed>/modules` for nested levels — this mirrors
/// git's per-worktree storage convention).
fn rewrite_url(
    cfg: &PoolConfig,
    submodule_name: &str,
    declared_url: &str,
    name_scope: &str,
) -> Option<String> {
    let mode = cfg.submodule_mirror_mode?;
    let base = cfg.submodule_mirror_base.as_ref()?;
    let composed_name = if name_scope.is_empty() {
        submodule_name.to_string()
    } else {
        format!("{name_scope}/{submodule_name}")
    };
    Some(match mode {
        SubmoduleMirrorMode::BareMirror => {
            // <base>/<org>/<repo>.git extracted from the declared URL.
            let org_repo = extract_org_repo(declared_url)?;
            format!("{}/{org_repo}.git", base.display())
        }
        SubmoduleMirrorMode::GitModules => {
            // <base>/.git/modules/<composed_name>
            format!("{}/.git/modules/{composed_name}", base.display())
        }
    })
}

/// Extract `<org>/<repo>` from `git@host:org/repo[.git]`, `scheme://host/org/repo[.git]`.
fn extract_org_repo(url: &str) -> Option<String> {
    let trimmed = url.strip_suffix(".git").unwrap_or(url);

    // URL-style first — contains `://`.
    if trimmed.contains("://") {
        let (_scheme, after) = trimmed.split_once("://")?;
        // After scheme: `host/path`. Take everything after the first `/`.
        let (_host, path) = after.split_once('/')?;
        return Some(path.to_string());
    }

    // SSH-style: `user@host:org/repo` (no `://`).
    if let Some((_pre, after_colon)) = trimmed.split_once(':') {
        return Some(after_colon.to_string());
    }

    None
}

/// Initialize and update submodules filtered by `exclude_tags` and with URL rewrites
/// applied. Recurses into nested `.gitmodules` if present. Each materialized submodule
/// (top-level + nested) gets a local branch named `branch_name` force-created at HEAD,
/// matching the parent slot's branch — gives commits in the submodule a push-ready label
/// and a stable ref for `wt land` to fetch by. See [[docs/wt.md#land-flow]].
pub fn update(
    slot: &Path,
    cfg: &PoolConfig,
    exclude_tags: &[String],
    branch_name: &str,
) -> Result<()> {
    update_recursive(slot, cfg, exclude_tags, branch_name, "")
}

fn update_recursive(
    dir: &Path,
    cfg: &PoolConfig,
    exclude_tags: &[String],
    branch_name: &str,
    name_scope: &str,
) -> Result<()> {
    let entries = parse_gitmodules(dir)?;
    if entries.is_empty() {
        return Ok(());
    }

    // Tag-based exclusion is meaningful only at top level (Unity package gating);
    // nested submodules always init regardless.
    let exclude_set: HashSet<&String> = exclude_tags.iter().collect();
    let (skipped, included): (Vec<_>, Vec<_>) = if name_scope.is_empty() {
        entries
            .into_iter()
            .partition(|e| e.tags.iter().any(|t| exclude_set.contains(t)))
    } else {
        (Vec::new(), entries)
    };

    // Init each included submodule via its OWN git process, in parallel. The classic
    // batched form (`git submodule update --init <p1> <p2>...`) parallelizes fetch via
    // `submodule.fetchJobs` but the parent process still serializes per-submodule
    // overhead (checkout dispatch, ref recording, LFS smudge). Running N sibling
    // processes lets the OS schedule everything fully concurrent.
    //
    // Two-phase to avoid `<source>/.git/config` lockfile contention:
    //   1. Sequentially write `submodule.<name>.url = <effective>` to the parent's
    //      shared config via plain `git config` — much cheaper than `git submodule init`,
    //      which spawns submodule--helper to do the same one write plus extra validation
    //      work (~50ms vs ~5ms per submodule). For 17 submodules that's ~750ms vs ~85ms.
    //      `submodule update <path>` doesn't need `submodule.<name>.active` when given
    //      an explicit path, so we skip that key.
    //   2. In parallel, `git submodule update <path>` — reads-only on parent config,
    //      writes to its own admin dir / working tree, so processes don't fight for
    //      the same lockfile.
    //
    // `protocol.file.allow=always` overrides git's post-CVE-2022-39253 default that
    // refuses `file:` (incl. plain local path) submodule clones — required for our
    // bare-mirror and git-modules modes.
    if !included.is_empty() {
        // Phase 1: serialized url registration. Top-level writes target
        // `<source>/.git/config`, which is shared across pools using the same
        // source repo. The per-source mutex (anchored on the source's gitdir)
        // prevents `O_EXCL` lockfile contention between parallel acquires.
        // Nested levels write to `<source>/.git/modules/<top>/config` (per
        // top-level), which siblings don't share — no mutex needed there.
        let source_mu = if name_scope.is_empty() {
            let source_gitdir = git::source_gitdir(&cfg.source)
                .context("resolving source gitdir for per-source config mutex")?;
            Some(mutex::FileLock::acquire(fs_paths::source_config_mutex(&source_gitdir))
                .context("acquiring per-source config mutex")?)
        } else {
            None
        };
        for e in &included {
            let effective_url = match rewrite_url(cfg, &e.name, &e.url, name_scope) {
                Some(u) => u,
                None => {
                    // No mirror configured: fall back to the declared URL. We skip
                    // `git submodule init`'s URL-resolution step (which we'd otherwise
                    // get for free), so refuse relative URLs here — they'd be written
                    // verbatim and silently fetch from the wrong place. (`git submodule
                    // init` resolves `../foo.git` against the parent's `origin` URL.)
                    if e.url.starts_with("./") || e.url.starts_with("../") {
                        bail!(
                            "submodule {} has relative url `{}` and no `submodule_mirror_*` \
                             rewrite is configured. Either configure a mirror in pool config, \
                             or change the submodule URL to absolute.",
                            e.name,
                            e.url
                        );
                    }
                    e.url.clone()
                }
            };
            git::run(
                dir,
                &[
                    "config",
                    &format!("submodule.{}.url", e.name),
                    &effective_url,
                ],
            )
            .with_context(|| {
                format!(
                    "writing submodule.{}.url in {}",
                    e.name,
                    dir.display()
                )
            })?;
        }
        drop(source_mu);

        // Phase 2: parallel `submodule update` + branch attach + nested recursion.
        // Each top-level worker handles its own subtree end-to-end so nested
        // levels parallelize alongside top-level siblings. Per-submodule git
        // processes write only to their own admin dir / working tree, so they
        // don't fight for the same lockfile. Inline-fallback on OS thread-create
        // failure: see `parallel` module.
        crate::parallel::try_for_each(&included, |e| {
            git::run_with_config(
                dir,
                &[("protocol.file.allow", "always")],
                &["submodule", "update", &e.path],
            )
            .map(drop)
            .with_context(|| {
                format!("submodule update {} in {}", e.path, dir.display())
            })?;
            let sub_path = dir.join(&e.path);
            git::checkout_force_branch(&sub_path, branch_name).with_context(|| {
                format!(
                    "creating branch '{}' in submodule {}",
                    branch_name,
                    sub_path.display()
                )
            })?;
            if sub_path.join(".gitmodules").exists() {
                let child_scope = if name_scope.is_empty() {
                    format!("{}/modules", e.name)
                } else {
                    format!("{name_scope}/{}/modules", e.name)
                };
                update_recursive(&sub_path, cfg, exclude_tags, branch_name, &child_scope)?;
            }
            Ok(())
        })?;
    }

    // Deinit + remove dirs for tag-excluded submodules. Unity treats absent embedded
    // packages as "not installed" — exactly what excluded modules should look like.
    for e in &skipped {
        let (ok, _, stderr) = git::run_lenient(dir, &["submodule", "deinit", "-f", &e.path])?;
        if !ok {
            // Don't fail the whole acquire on deinit cleanup — but surface the warning
            // so an operator debugging missing exclusion has a breadcrumb.
            eprintln!("warn: submodule deinit -f {} failed: {}", e.path, stderr);
        }
        let _ = std::fs::remove_dir_all(dir.join(&e.path));
    }
    Ok(())
}

/// Best-effort recursive cleanup of the per-slot `<branch_name>` branch left by
/// `update`. Detaches HEAD and deletes the branch in every submodule (incl.
/// nested) so the slot's submodule gitdirs end the cycle as clean as the parent.
/// Tag-excluded (deinit'd) submodules are silently skipped — their working dir
/// is empty so the git ops fail lenient.
///
/// Parallel per-level: each submodule's gitdir is independent, so detach +
/// branch-delete fan out without contention. Failures are absorbed (mirrors
/// the parent's best-effort branch cleanup in `release.rs`).
pub fn delete_branch_recursive(dir: &Path, branch_name: &str) {
    let Ok(entries) = parse_gitmodules(dir) else {
        return;
    };
    if entries.is_empty() {
        return;
    }

    // Inline-fallback on OS thread-create failure: see `parallel` module.
    crate::parallel::for_each(&entries, |e| {
        let sub_path = dir.join(&e.path);
        let _ = git::detach_head(&sub_path);
        let _ = git::branch_delete(&sub_path, branch_name);
    });

    for e in &entries {
        let child = dir.join(&e.path);
        if child.join(".gitmodules").exists() {
            delete_branch_recursive(&child, branch_name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gitmodules_basic() {
        let raw = "\
submodule.foo.path=path/to/foo
submodule.foo.url=git@github.com:org/foo.git
submodule.foo.worktreepooltag=editor
submodule.bar.path=bar
submodule.bar.url=https://github.com/org/bar.git
";
        let mut entries = parse_gitmodules_text(raw).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "bar");
        assert_eq!(entries[1].name, "foo");
        assert_eq!(entries[1].tags, vec!["editor".to_string()]);
        assert!(entries[0].tags.is_empty());
    }

    #[test]
    fn parse_gitmodules_multi_tag() {
        let raw = "\
submodule.foo.path=foo
submodule.foo.url=u
submodule.foo.worktreepooltag=editor
submodule.foo.worktreepooltag=optional
";
        let entries = parse_gitmodules_text(raw).unwrap();
        assert_eq!(entries[0].tags, vec!["editor", "optional"]);
    }

    #[test]
    fn extract_org_repo_forms() {
        assert_eq!(
            extract_org_repo("git@github.com:org/repo.git").as_deref(),
            Some("org/repo")
        );
        assert_eq!(
            extract_org_repo("https://github.com/org/repo.git").as_deref(),
            Some("org/repo")
        );
        assert_eq!(
            extract_org_repo("git://git.studioboxcat.com/org/repo.git").as_deref(),
            Some("org/repo")
        );
        assert_eq!(extract_org_repo("totally-not-a-url"), None);
    }

    #[test]
    fn rewrite_url_bare_mirror() {
        let cfg = PoolConfig {
            schema_version: 1,
            source: "/x".into(),
            default_commit: "main".into(),
            max_slots: 4,
            groups: vec![],
            submodule_mirror_mode: Some(SubmoduleMirrorMode::BareMirror),
            submodule_mirror_base: Some("/mirrors".into()),
        };
        assert_eq!(
            rewrite_url(&cfg, "name", "git@github.com:org/repo.git", "").as_deref(),
            Some("/mirrors/org/repo.git")
        );
    }

    #[test]
    fn rewrite_url_git_modules() {
        let cfg = PoolConfig {
            schema_version: 1,
            source: "/x".into(),
            default_commit: "main".into(),
            max_slots: 4,
            groups: vec![],
            submodule_mirror_mode: Some(SubmoduleMirrorMode::GitModules),
            submodule_mirror_base: Some("/Users/foo/meow-tower".into()),
        };
        assert_eq!(
            rewrite_url(&cfg, "Packages/com.foo", "git@github.com:org/foo.git", "").as_deref(),
            Some("/Users/foo/meow-tower/.git/modules/Packages/com.foo")
        );
        // Nested
        assert_eq!(
            rewrite_url(&cfg, "child", "git@github.com:org/child.git", "Packages/com.foo/modules")
                .as_deref(),
            Some("/Users/foo/meow-tower/.git/modules/Packages/com.foo/modules/child")
        );
    }
}
