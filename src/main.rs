//! See README.md for the design spec.
mod acquire;
mod admin;
mod atomic;
mod cli;
mod config;
mod dashboard;
mod doctor;
mod fs_paths;
mod git;
mod lock;
mod mutex;
mod release;
mod slot;
mod submodules;
mod yaml;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    if let Err(e) = run(cli) {
        // `{:#}` flattens the anyhow context chain; readable for CLI users.
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: cli::Cli) -> Result<()> {
    use cli::Command::*;

    // Doctor is the only subcommand that doesn't require --pool.
    if matches!(cli.command, Doctor) {
        return doctor::run();
    }

    let pool_key = cli
        .pool
        .as_deref()
        .ok_or_else(|| anyhow!("--pool is required for this subcommand"))?;
    let pool_path = fs_paths::pool_root(pool_key);

    match cli.command {
        Init(args) => cmd_init(pool_key, &pool_path, args),
        cmd => {
            if !fs_paths::pool_config(&pool_path).exists() {
                bail!(
                    "pool '{pool_key}' not initialized at {}; run: worktree-pool --pool {pool_key} init --source <repo> --max-slots <n> ...",
                    pool_path.display()
                );
            }
            let cfg = config::load(&pool_path)
                .with_context(|| format!("loading config for pool '{pool_key}'"))?;
            dispatch(pool_key, &pool_path, &cfg, cmd)
        }
    }
}

fn cmd_init(pool_key: &str, pool_path: &std::path::Path, args: cli::InitArgs) -> Result<()> {
    if fs_paths::pool_config(pool_path).exists() {
        bail!(
            "pool '{pool_key}' already initialized at {}",
            pool_path.display()
        );
    }
    let cfg = config::PoolConfig::from_init_args(&args)?;
    config::write(pool_path, &cfg).context("writing pool config")?;
    eprintln!(
        "initialized pool '{pool_key}' at {} (source: {})",
        pool_path.display(),
        cfg.source.display()
    );
    Ok(())
}

fn dispatch(
    _pool_key: &str,
    pool_path: &std::path::Path,
    cfg: &config::PoolConfig,
    cmd: cli::Command,
) -> Result<()> {
    use cli::Command::*;
    match cmd {
        Init(_) | Doctor => unreachable!("handled in run()"),
        Acquire(args) => acquire::run(pool_path, cfg, args),
        Release(args) => release::run(pool_path, cfg, args),
        Ls(args) => dashboard::ls(pool_path, cfg, args),
        Inspect(args) => dashboard::inspect(pool_path, cfg, args),
        Unstick(args) => admin::unstick(pool_path, cfg, args),
        ValidateGitmodules => admin::validate_gitmodules(pool_path, cfg),
    }
}
