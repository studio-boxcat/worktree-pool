//! CLI surface. See CLAUDE.md for command semantics.
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "worktree-pool", version, about, long_about = None)]
pub struct Cli {
    /// Pool key. Path resolves to `$WORKTREE_ROOT/<key>/`.
    /// Required for all subcommands except `doctor`.
    #[arg(short, long, global = true)]
    pub pool: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize a new pool. Writes `<pool>/.meta/config.yaml`.
    Init(InitArgs),

    /// Acquire a slot. **Stdout: exactly one line, the canonical slot path.**
    /// All logs/warnings/errors go to stderr. Callers can read stdout directly
    /// — `tail -1` is defensive but redundant.
    Acquire(AcquireArgs),

    /// Release a held slot back to idle. Idempotent.
    Release(ReleaseArgs),

    /// List slots in this pool.
    Ls(LsArgs),

    /// Dump lock + git state for a held slot.
    Inspect(InspectArgs),

    /// Print the canonical filesystem path of the slot whose branch matches
    /// NAME. Exit 0 if found, 1 if no held slot has that branch.
    Path(PathArgs),

    /// Clear stale init mutexes.
    Unstick(UnstickArgs),

    /// Parse `.gitmodules`; warn on unknown `worktreePool*` keys.
    ValidateGitmodules,

    /// Host-level health check. Does not require `--pool`.
    Doctor,
}

#[derive(clap::Args, Debug)]
pub struct InitArgs {
    /// Source git repo path (bare or working clone).
    #[arg(long)]
    pub source: PathBuf,

    /// Default commit-ish for `acquire --commit` when omitted.
    /// Plain `main` resolves to `refs/heads/main` on bare repos and the local main on
    /// working clones — works in both cases. Override per pool if your default branch differs.
    #[arg(long, default_value = "main")]
    pub default_commit: String,

    /// Slot count cap per group (or total when no groups).
    #[arg(long)]
    pub max_slots: u32,

    /// Optional comma-separated group names.
    #[arg(long, value_delimiter = ',')]
    pub groups: Vec<String>,

    /// Submodule URL rewrite mode.
    #[arg(long, value_enum)]
    pub submodule_mirror_mode: Option<SubmoduleMirrorMode>,

    /// Base path for submodule mirror.
    #[arg(long)]
    pub submodule_mirror_base: Option<PathBuf>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmoduleMirrorMode {
    BareMirror,
    GitModules,
}

impl SubmoduleMirrorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BareMirror => "bare-mirror",
            Self::GitModules => "git-modules",
        }
    }
}

#[derive(clap::Args, Debug)]
pub struct AcquireArgs {
    /// Branch name created in the slot. Identifies the slot for release/inspect.
    pub name: String,

    /// Commit-ish to check out. Default: pool's `default_commit`.
    #[arg(long)]
    pub commit: Option<String>,

    /// Group to acquire from. Required if pool has groups; defaults to first.
    #[arg(long)]
    pub group: Option<String>,

    /// Comma-separated `worktreePoolTag` values to deinit/skip during submodule update.
    #[arg(long, value_delimiter = ',')]
    pub exclude_submodule_tags: Vec<String>,

    /// Refuse if any held slot already has the same resolved full SHA.
    /// Use for CI builds (avoid duplicate work). Omit for dev sessions
    /// (common case: dev branches off main while a build is also at main).
    #[arg(long)]
    pub unique_sha: bool,
}

#[derive(clap::Args, Debug)]
pub struct ReleaseArgs {
    /// Branch name of the held slot to release.
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct LsArgs {
    /// Spawn `git status --porcelain` + `rev-list` per held slot for dirty/untracked/ahead columns.
    #[arg(long)]
    pub git_status: bool,
}

#[derive(clap::Args, Debug)]
pub struct InspectArgs {
    /// Branch name of the held slot to inspect.
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct PathArgs {
    /// Branch name of the held slot.
    pub name: String,
}

#[derive(clap::Args, Debug)]
pub struct UnstickArgs {
    /// Specific slot id (e.g. `ios-2`). Without it, report all init mutexes.
    #[arg(long)]
    pub slot: Option<String>,
}
