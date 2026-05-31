//! Pool configuration written by `init` to `<pool>/.meta/config.yaml`.
use anyhow::{Context, Result, anyhow, bail};
use std::path::{Path, PathBuf};

use crate::types::{CommitIsh, GroupName};
use crate::{atomic, cli, fs_paths, yaml};

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub schema_version: u32,
    pub source: PathBuf,
    pub default_commit: CommitIsh,
    pub max_slots: u32,
    pub groups: Vec<GroupName>,
    pub submodule_mirror_mode: Option<cli::SubmoduleMirrorMode>,
    pub submodule_mirror_base: Option<PathBuf>,
}

const SCHEMA_VERSION: u32 = 1;

impl PoolConfig {
    pub fn from_init_args(args: &cli::InitArgs) -> Result<Self> {
        if args.max_slots == 0 {
            bail!("--max-slots must be > 0");
        }
        if !args.source.exists() {
            bail!("--source path does not exist: {}", args.source.display());
        }
        if args.submodule_mirror_mode.is_some() != args.submodule_mirror_base.is_some() {
            bail!("--submodule-mirror-mode and --submodule-mirror-base must be set together");
        }
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            source: args.source.clone(),
            default_commit: CommitIsh::from(args.default_commit.as_str()),
            max_slots: args.max_slots,
            groups: args.groups.iter().map(|g| GroupName::from(g.as_str())).collect(),
            submodule_mirror_mode: args.submodule_mirror_mode,
            submodule_mirror_base: args.submodule_mirror_base.clone(),
        })
    }

    fn serialize(&self) -> String {
        let mode = self
            .submodule_mirror_mode
            .map(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let base = self
            .submodule_mirror_base
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        yaml::serialize(&[
            ("schema_version", self.schema_version.to_string()),
            ("source", self.source.display().to_string()),
            ("default_commit", self.default_commit.as_str().to_string()),
            ("max_slots", self.max_slots.to_string()),
            ("groups", self.groups.iter().map(GroupName::as_str).collect::<Vec<_>>().join(",")),
            ("submodule_mirror_mode", mode),
            ("submodule_mirror_base", base),
        ])
    }

    fn parse(text: &str) -> Result<Self> {
        let map = yaml::parse(text);
        // `.context()` keeps the underlying ParseIntError as the error source
        // rather than dropping it with `.ok()`, and splits missing vs. malformed.
        let schema_version: u32 = map
            .get("schema_version")
            .ok_or_else(|| anyhow!("config missing `schema_version`"))?
            .parse()
            .context("config has invalid `schema_version` (want a u32)")?;
        if schema_version != SCHEMA_VERSION {
            bail!("config schema_version {schema_version} unsupported (expected {SCHEMA_VERSION})");
        }
        let source = map
            .get("source")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("config missing `source`"))?;
        let default_commit = map
            .get("default_commit")
            .filter(|s| !s.is_empty())
            .map(|s| CommitIsh::from(s.as_str()))
            .ok_or_else(|| anyhow!("config missing `default_commit`"))?;
        let max_slots: u32 = map
            .get("max_slots")
            .ok_or_else(|| anyhow!("config missing `max_slots`"))?
            .parse()
            .context("config has invalid `max_slots` (want a u32)")?;
        let groups = map
            .get("groups")
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(str::trim).map(GroupName::from).collect())
            .unwrap_or_default();
        let submodule_mirror_mode = match map.get("submodule_mirror_mode").map(String::as_str) {
            Some("bare-mirror") => Some(cli::SubmoduleMirrorMode::BareMirror),
            Some("git-modules") => Some(cli::SubmoduleMirrorMode::GitModules),
            Some("") | None => None,
            Some(other) => bail!("invalid submodule_mirror_mode: {other:?}"),
        };
        let submodule_mirror_base = map
            .get("submodule_mirror_base")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        if submodule_mirror_mode.is_some() != submodule_mirror_base.is_some() {
            bail!("submodule_mirror_mode and submodule_mirror_base must be set together");
        }
        Ok(Self {
            schema_version,
            source,
            default_commit,
            max_slots,
            groups,
            submodule_mirror_mode,
            submodule_mirror_base,
        })
    }
}

pub fn write(pool_path: &Path, cfg: &PoolConfig) -> Result<()> {
    atomic::write(&fs_paths::pool_config(pool_path), &cfg.serialize())
}

pub fn load(pool_path: &Path) -> Result<PoolConfig> {
    let path = fs_paths::pool_config(pool_path);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading config {}", path.display()))?;
    PoolConfig::parse(&text)
        .with_context(|| format!("parsing config {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_args(tmp: &TempDir) -> cli::InitArgs {
        // Create a fake source dir so `from_init_args` validates existence.
        let src = tmp.path().join("source");
        std::fs::create_dir(&src).unwrap();
        cli::InitArgs {
            source: src,
            default_commit: "refs/remotes/origin/main".into(),
            max_slots: 16,
            groups: vec!["ios".into(), "android".into()],
            submodule_mirror_mode: Some(cli::SubmoduleMirrorMode::GitModules),
            submodule_mirror_base: Some(tmp.path().join("source")),
        }
    }

    #[test]
    fn round_trip() {
        let tmp = TempDir::new().unwrap();
        let cfg = PoolConfig::from_init_args(&sample_args(&tmp)).unwrap();
        let s = cfg.serialize();
        let parsed = PoolConfig::parse(&s).unwrap();
        assert_eq!(parsed.max_slots, 16);
        assert_eq!(parsed.groups, vec![GroupName::from("ios"), GroupName::from("android")]);
        assert_eq!(parsed.default_commit, CommitIsh::from("refs/remotes/origin/main"));
        assert_eq!(
            parsed.submodule_mirror_mode,
            Some(cli::SubmoduleMirrorMode::GitModules)
        );
    }

    #[test]
    fn rejects_zero_max_slots() {
        let tmp = TempDir::new().unwrap();
        let mut args = sample_args(&tmp);
        args.max_slots = 0;
        assert!(PoolConfig::from_init_args(&args).is_err());
    }

    #[test]
    fn rejects_unset_source() {
        let tmp = TempDir::new().unwrap();
        let mut args = sample_args(&tmp);
        args.source = tmp.path().join("does-not-exist");
        assert!(PoolConfig::from_init_args(&args).is_err());
    }

    #[test]
    fn rejects_partial_submodule_mirror() {
        let tmp = TempDir::new().unwrap();
        let mut args = sample_args(&tmp);
        args.submodule_mirror_base = None;
        assert!(PoolConfig::from_init_args(&args).is_err());
    }

    #[test]
    fn write_then_load() {
        let tmp = TempDir::new().unwrap();
        let pool = tmp.path().join("pool");
        let cfg = PoolConfig::from_init_args(&sample_args(&tmp)).unwrap();
        write(&pool, &cfg).unwrap();
        let loaded = load(&pool).unwrap();
        assert_eq!(loaded.max_slots, cfg.max_slots);
        assert_eq!(loaded.groups, cfg.groups);
    }
}
