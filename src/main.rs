//! See CLAUDE.md for the design spec.
mod acquire;
mod admin;
mod atomic;
mod cli;
mod config;
mod dashboard;
mod doctor;
mod exit;
mod fs_paths;
mod git;
mod mutex;
mod parallel;
mod release;
mod slot;
mod submodules;
mod types;
mod yaml;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    if let Err(e) = run(cli) {
        // `{:#}` flattens the anyhow context chain; readable for CLI users.
        eprintln!("error: {e:#}");
        let code = e
            .chain()
            .find_map(|c| c.downcast_ref::<exit::ExitKind>().copied())
            .map_or(1, |k| k.code());
        std::process::exit(code);
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
            dispatch(&pool_path, &cfg, cmd)
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
    let groups = if cfg.groups.is_empty() {
        "none".to_string()
    } else {
        cfg.groups.iter().map(types::GroupName::as_str).collect::<Vec<_>>().join(",")
    };
    let mirror = match (cfg.submodule_mirror_mode, &cfg.submodule_mirror_base) {
        (Some(m), Some(b)) => format!(", submodule_mirror={}@{}", m.as_str(), b.display()),
        _ => String::new(),
    };
    eprintln!(
        "initialized pool '{pool_key}' at {} (source={}, max_slots={}, groups={groups}, default_commit={}{mirror})",
        pool_path.display(),
        cfg.source.display(),
        cfg.max_slots,
        cfg.default_commit,
    );
    Ok(())
}

fn dispatch(
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
        Path(args) => dashboard::path(pool_path, cfg, args),
        Unstick(args) => admin::unstick(pool_path, args),
        ValidateGitmodules => admin::validate_gitmodules(cfg),
    }
}
