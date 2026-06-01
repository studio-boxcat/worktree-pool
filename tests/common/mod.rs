//! Shared harness for the integration tests against a real bare-repo fixture.
//! Spawn the built binary. Parallel-safe: each test owns a unique pool key
//! (`pool_key`) and its own source fixture, so the default multi-threaded
//! `cargo test` runner is fine — no `--test-threads=1` needed. Cargo does NOT
//! treat `tests/common/mod.rs` as a test target, so it's the idiomatic place
//! for helpers shared across the per-domain test files.
//
// Each test binary (lifecycle/submodules/session) compiles this module
// independently and uses only the subset of helpers it needs, so unused-fn
// warnings here are expected noise, not dead code — silence them.
#![allow(dead_code)]
use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::SystemTime;

// ---------- helpers ----------

pub fn pool_key() -> String {
    // Must be unique per call. `cargo test` runs every test as a thread in ONE
    // process, so `pid` is constant across them — uniqueness then rests entirely
    // on `nonce`, and two parallel tests calling this in the same `SystemTime`
    // tick collide, aliasing their pool dirs. The damage isn't subtle: each test's
    // `Cleanup` does `remove_dir_all(pool_root(key))` on drop, so the first to
    // finish deletes the other's *live* pool mid-run → random capacity/acquire
    // failures. The atomic counter makes intra-process uniqueness total; pid +
    // nonce keep it unique across separate test processes and repeat runs.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("wtp-test-{pid}-{seq}-{nonce}")
}

/// Isolated `WORKTREE_ROOT` for the whole test process. Tests must NOT touch the
/// operator's real `~/.worktree-pool`: a crashed/killed test would leak
/// `wtp-test-*` dirs next to live pools, and test pools would share a directory
/// with real ones. A per-pid temp dir keeps concurrent `cargo test` runs (and
/// the unit-test binary) from colliding; per-test `Cleanup` removes each pool
/// under it. Every binary spawn routes through `wtp()` / `session_cmd*`, which
/// point the subprocess at this root via the `WORKTREE_ROOT` env.
static TEST_ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let root = std::env::temp_dir().join(format!("wtp-test-root-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create isolated test WORKTREE_ROOT");
    root
});

fn test_root() -> &'static Path {
    TEST_ROOT.as_path()
}

pub fn pool_root(key: &str) -> PathBuf {
    test_root().join(key)
}

/// `worktree-pool` binary command with the isolated `WORKTREE_ROOT` preset — the
/// single choke point so no spawn ever reads the operator's real root.
pub fn wtp() -> Command {
    let mut cmd = Command::cargo_bin("worktree-pool").unwrap();
    cmd.env("WORKTREE_ROOT", test_root());
    cmd
}

pub fn run_git(cwd: &Path, args: &[&str]) {
    let st = StdCommand::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap();
    assert!(st.success(), "git {} failed in {}", args.join(" "), cwd.display());
}

pub fn run_git_root(args: &[&str]) {
    StdCommand::new("git").args(args).status().unwrap();
}

pub fn assert_ok(out: &std::process::Output, ctx: &str) {
    if out.status.success() { return; }
    let sep = if ctx.is_empty() { "" } else { "\n" };
    panic!("{ctx}{sep}stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr));
}

/// Extract the slot path from a successful acquire's stdout (stdout → trim →
/// `PathBuf`). Centralizes the one-liner the lifecycle/submodule/session tests
/// all repeat after every acquire.
pub fn output_to_slot_path(out: &std::process::Output) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// Commits *all* pending changes (`add -A`) — fixtures and slot tests write a
// single target file in an otherwise-clean tree; tests with stray uncommitted
// state would silently capture it. Callers do NOT need to `git add` first.
pub fn git_commit(cwd: &Path, msg: &str) {
    let st = StdCommand::new("git").current_dir(cwd).args(["add", "-A"]).status().unwrap();
    assert!(st.success(), "git add -A failed in {}", cwd.display());
    let st = StdCommand::new("git")
        .current_dir(cwd)
        .args(["commit", "--quiet", "-m", msg])
        .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
        .status().unwrap();
    assert!(st.success(), "git commit -m {msg:?} failed in {}", cwd.display());
}

pub fn make_fixture(dir: &Path) -> PathBuf {
    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone",
        "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    git_commit(&staging, "initial");
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

pub struct Cleanup(pub String);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(pool_root(&self.0));
    }
}

pub fn init_pool(key: &str, bare: &Path) {
    wtp()
        .args(["--pool", key, "init"])
        .arg("--source")
        .arg(bare)
        .args(["--max-slots", "4", "--groups", "ios,android"])
        .assert()
        .success();
}

pub fn init_pool_groupless(key: &str, bare: &Path) {
    wtp()
        .args(["--pool", key, "init"])
        .arg("--source")
        .arg(bare)
        .args(["--max-slots", "4"])
        .assert()
        .success();
}

pub fn acquire_dev(key: &str, name: &str) -> std::process::Output {
    wtp()
        .args(["--pool", key, "acquire", name, "--group", "ios"])
        .output()
        .unwrap()
}

pub fn acquire_dev_sub(key: &str, name: &str) -> std::process::Output {
    wtp()
        .args(["--pool", key, "acquire", name, "--group", "ios"])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap()
}

pub fn release(key: &str, name: &str) {
    wtp()
        .args(["--pool", key, "release", name])
        .assert()
        .success();
}

/// Extract the canonical slot id (basename of acquire's stdout path).
pub fn slot_id_from_output(out: &std::process::Output) -> String {
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    PathBuf::from(&path).file_name().unwrap().to_string_lossy().into_owned()
}

pub fn init_pool_groupless_max1(key: &str, bare: &Path) {
    wtp()
        .args(["--pool", key, "init"])
        .arg("--source")
        .arg(bare)
        .args(["--max-slots", "1"])
        .assert()
        .success();
}

pub fn acquire_dev_groupless(key: &str, name: &str) -> std::process::Output {
    wtp()
        .args(["--pool", key, "acquire", name])
        .output()
        .unwrap()
}

// ---------- submodule fixtures ----------

/// Build a standalone bare repo at `<dir>/<name>.git` with a single committed
/// `FILE` containing `content`; returns the bare path. The leaf submodule-source
/// pattern shared by the submodule fixture builders below.
pub fn make_sub_bare(dir: &Path, name: &str, content: &str) -> PathBuf {
    let sub_bare = dir.join(format!("{name}.git"));
    run_git_root(&["init", "--quiet", "--bare", &sub_bare.display().to_string()]);
    let sub_staging = dir.join(format!("{name}-staging"));
    run_git_root(&[
        "clone",
        "--quiet",
        &sub_bare.display().to_string(),
        &sub_staging.display().to_string(),
    ]);
    std::fs::write(sub_staging.join("FILE"), content.as_bytes()).unwrap();
    git_commit(&sub_staging, "init");
    run_git(&sub_staging, &["push", "--quiet", "-u", "origin", "main"]);
    sub_bare
}

/// Make a bare repo whose tip commit registers a submodule. Returns the parent bare.
///
/// Layout:
///   <dir>/sub.git           (bare submodule source, one commit with "sub-content")
///   <dir>/source.git        (bare parent source, one commit with .gitmodules + sub/)
pub fn make_fixture_with_submodule(dir: &Path) -> PathBuf {
    // Submodule source.
    let sub_bare = make_sub_bare(dir, "sub", "sub-content");

    // Parent source.
    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone",
        "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    run_git(&staging, &["add", "README"]);
    // Allow `file://` submodule URLs (git 2.38+ disables by default).
    run_git(
        &staging,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            &sub_bare.display().to_string(),
            "sub",
        ],
    );
    git_commit(&staging, "with submodule");
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

/// Make a parent bare whose top-level submodule `outer` itself registers a
/// nested submodule `inner`. Exercises the full-depth parallel-recursion
/// path in `submodules::update_recursive`.
///
/// Layout:
///   <dir>/inner.git    (innermost bare, one commit)
///   <dir>/outer.git    (registers inner as a submodule at `inner/`)
///   <dir>/source.git   (registers outer as a submodule at `outer/`)
pub fn make_fixture_with_nested_submodule(dir: &Path) -> PathBuf {
    // Innermost bare.
    let inner_bare = make_sub_bare(dir, "inner", "inner-content");

    // Outer bare with nested inner.
    let outer_bare = dir.join("outer.git");
    run_git_root(&["init", "--quiet", "--bare", &outer_bare.display().to_string()]);
    let outer_staging = dir.join("outer-staging");
    run_git_root(&[
        "clone", "--quiet",
        &outer_bare.display().to_string(),
        &outer_staging.display().to_string(),
    ]);
    std::fs::write(outer_staging.join("OUTER_FILE"), b"outer-content").unwrap();
    run_git(&outer_staging, &["add", "OUTER_FILE"]);
    run_git(
        &outer_staging,
        &[
            "-c", "protocol.file.allow=always",
            "submodule", "add", "--quiet",
            &inner_bare.display().to_string(),
            "inner",
        ],
    );
    git_commit(&outer_staging, "outer with inner");
    run_git(&outer_staging, &["push", "--quiet", "-u", "origin", "main"]);

    // Source bare registers outer.
    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone", "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    run_git(&staging, &["add", "README"]);
    run_git(
        &staging,
        &[
            "-c", "protocol.file.allow=always",
            "submodule", "add", "--quiet",
            &outer_bare.display().to_string(),
            "outer",
        ],
    );
    // Recursively init in the working clone so its `.git/modules/outer/modules/inner`
    // object store exists — the `source-submodules` mirror base resolves the nested inner
    // from there. `submodule add outer` alone doesn't descend into inner.
    run_git(
        &staging,
        &["-c", "protocol.file.allow=always", "submodule", "update", "--init", "--recursive"],
    );
    git_commit(&staging, "with nested outer");
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

/// Make a bare repo with two submodules; `editor_sub` is tagged
/// `worktreePoolTag = editor` so `--exclude-submodule-tags editor` skips it.
pub fn make_fixture_with_tagged_submodules(dir: &Path) -> PathBuf {
    let runtime_bare = make_sub_bare(dir, "runtime", "runtime-content");
    let editor_bare = make_sub_bare(dir, "editor", "editor-content");

    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone", "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    run_git(&staging, &["add", "README"]);
    run_git(
        &staging,
        &[
            "-c", "protocol.file.allow=always",
            "submodule", "add", "--quiet",
            &runtime_bare.display().to_string(),
            "runtime_sub",
        ],
    );
    run_git(
        &staging,
        &[
            "-c", "protocol.file.allow=always",
            "submodule", "add", "--quiet",
            &editor_bare.display().to_string(),
            "editor_sub",
        ],
    );
    // Tag the editor submodule.
    run_git(
        &staging,
        &[
            "config", "-f", ".gitmodules",
            "submodule.editor_sub.worktreePoolTag", "editor",
        ],
    );
    git_commit(&staging, "with tagged submodules");
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

/// Make a bare repo whose tip commit registers `n` independent submodules.
/// Each submodule is a tiny standalone bare. Returns the parent bare.
pub fn make_fixture_with_n_submodules(dir: &Path, n: usize) -> PathBuf {
    // Build N tiny submodule bares.
    let mut sub_bares = Vec::new();
    for i in 0..n {
        sub_bares.push(make_sub_bare(dir, &format!("sub{i}"), &format!("sub{i}-content")));
    }

    // Parent source with N submodules.
    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone",
        "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    run_git(&staging, &["add", "README"]);
    for (i, sub_bare) in sub_bares.iter().enumerate() {
        run_git(
            &staging,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                &sub_bare.display().to_string(),
                &format!("sub{i}"),
            ],
        );
    }
    git_commit(&staging, "with submodules");
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

// ---------- hook fixture ----------

/// Like `make_fixture`, but also commits a `.wt-hooks.sh` carrying `hook_body`
/// into the source. Core reads the hook from the *slot checkout* (not the
/// source worktree) — so the file MUST be committed to be present after acquire.
/// This is precisely why slot-read works for bare sources: the bare `source.git`
/// has no working tree, but the committed file lands in every slot checkout.
pub fn make_fixture_with_hook(dir: &Path, hook_body: &str) -> PathBuf {
    let bare = dir.join("source.git");
    run_git_root(&["init", "--quiet", "--bare", &bare.display().to_string()]);
    let staging = dir.join("staging");
    run_git_root(&[
        "clone",
        "--quiet",
        &bare.display().to_string(),
        &staging.display().to_string(),
    ]);
    std::fs::write(staging.join("README"), b"hi").unwrap();
    std::fs::write(staging.join(".wt-hooks.sh"), hook_body.as_bytes()).unwrap();
    git_commit(&staging, "initial with hook");
    run_git(&staging, &["push", "--quiet", "-u", "origin", "main"]);
    bare
}

// ---------- session (wt bash wrapper) helpers ----------

pub fn session_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin/wt")
}

/// Build a `bash <wt> --pool <key> <verb-args...>` command with PATH augmented
/// so the script can resolve `worktree-pool` to the test binary, plus a no-op
/// SHELL so an unexpected `cmd_go` launch succeeds quietly. That lets cmd_go
/// tests rely on cmd_go's *own* exit code as the signal — without it,
/// /usr/bin/zsh would fire and muddy the stderr assertions.
pub fn session_cmd(key: &str, args: &[&str]) -> StdCommand {
    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_worktree-pool"));
    let bin_dir = bin_path.parent().unwrap();
    let prev_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.display(), prev_path);
    let mut cmd = StdCommand::new("bash");
    cmd.arg(session_script())
        .args(["--pool", key])
        .args(args)
        .env("PATH", new_path)
        .env("WORKTREE_ROOT", test_root());
    cmd.env("SHELL", "/usr/bin/true");
    // Tests run without a TTY; bypass cmd_go's TTY guard so we exercise the
    // launcher-misfire and ghost-slot paths it protects.
    cmd.env("WT_GO_ALLOW_NOTTY", "1");
    cmd
}

/// Acquire a slot, then break it. `gitlink_only=true` mirrors the most common
/// ghost-dir state (the gitlink file is gone, source-repo admin still around);
/// `false` is the harder case where the source-repo admin was also pruned —
/// matters because git's upward-walk on a missing `.git` could otherwise let
/// `slot_repo_ok` falsely pass (e.g. dotfiles repo at $HOME).
///
/// Returns (slot_path, canonical_slot_id). Under canonical-only naming, wt's
/// dir-based lookups (cmd_go/cmd_release/cmd_cleanup) key off the slot id.
pub fn acquire_then_break(key: &str, name: &str, gitlink_only: bool) -> (PathBuf, String) {
    let out = acquire_dev(key, name);
    assert_ok(&out, "acquire failed");
    let path = output_to_slot_path(&out);
    let slot_id = slot_id_from_output(&out);
    assert!(path.exists());
    let gitlink = std::fs::read_to_string(path.join(".git")).expect("read gitlink");
    let admin = PathBuf::from(gitlink.strip_prefix("gitdir: ").unwrap().trim());
    std::fs::remove_file(path.join(".git")).expect("remove slot .git gitlink");
    if !gitlink_only {
        std::fs::remove_dir_all(&admin).expect("remove source-repo admin");
    }
    (path, slot_id)
}

/// Acquire a slot and dirty its working tree (tracked + untracked changes).
/// Returns (slot_path, canonical_slot_id).
pub fn acquire_then_dirty(key: &str, name: &str) -> (PathBuf, String) {
    let out = acquire_dev(key, name);
    assert_ok(&out, "acquire failed");
    let path = output_to_slot_path(&out);
    let slot_id = slot_id_from_output(&out);
    std::fs::write(path.join("README"), b"dirty tracked\n").unwrap();
    std::fs::write(path.join("untracked.txt"), b"new\n").unwrap();
    (path, slot_id)
}

/// Build a `bash <wt> <verb-args>` command rooted at `cwd`, used for verbs that
/// auto-resolve the pool key from cwd (e.g. `wt land`, which takes no `--pool`).
pub fn session_cmd_cwd(cwd: &Path, args: &[&str]) -> StdCommand {
    let bin_path = PathBuf::from(env!("CARGO_BIN_EXE_worktree-pool"));
    let bin_dir = bin_path.parent().unwrap();
    let prev_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.display(), prev_path);
    let mut cmd = StdCommand::new("bash");
    cmd.arg(session_script())
        .args(args)
        .current_dir(cwd)
        .env("PATH", new_path)
        .env("SHELL", "/usr/bin/true")
        .env("WORKTREE_ROOT", test_root());
    cmd
}

// ---------- gitdir + capture helpers ----------

pub fn slot_gitdir_path(slot: &Path) -> PathBuf {
    let text = std::fs::read_to_string(slot.join(".git")).unwrap();
    let rest = text.strip_prefix("gitdir: ").unwrap().trim();
    PathBuf::from(rest)
}

pub fn run_git_capture(cwd: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {} failed: {}",
        args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ---------- submodule mirror init ----------

/// `init` like `init_pool` but configures a `source-submodules` submodule mirror.
/// Required now that a submodule-bearing pool must resolve submodules from a
/// local mirror (no declared-URL fallback). `base` is the working clone whose
/// `.git/modules/<name>` object stores serve the slots; it may differ from
/// `source` (e.g. a bare source mirrored from its sibling working clone).
pub fn init_pool_mirror(key: &str, source: &Path, base: &Path) {
    wtp()
        .args(["--pool", key, "init"])
        .arg("--source")
        .arg(source)
        .args([
            "--max-slots", "4", "--groups", "ios,android",
            "--submodule-mirror-mode", "source-submodules", "--submodule-mirror-base",
        ])
        .arg(base)
        .assert()
        .success();
}
