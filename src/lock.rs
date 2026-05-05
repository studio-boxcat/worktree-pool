//! Slot held marker. Lives at `<source>/.git/worktrees/<id>/worktree-pool/lock`.
//! Mere file presence indicates "held"; body is 1-3 lines of YAML carrying
//! `started_at`, `full_sha`, optional `group`. Format mirrors the wt-cleanup
//! grep contract — see `yaml.rs`.
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;

use crate::{atomic, yaml};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    pub started_at: DateTime<Utc>,
    pub full_sha: String,
    /// Group namespace this slot belongs to. None for groupless pools.
    pub group: Option<String>,
}

impl Lock {
    pub fn new(full_sha: String, group: Option<String>) -> Self {
        Self {
            started_at: Utc::now(),
            full_sha,
            group,
        }
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        atomic::write(path, &self.serialize())
    }

    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading lock {}", path.display()))?;
        Self::parse(&text)
            .with_context(|| format!("parsing lock {}", path.display()))
    }

    fn serialize(&self) -> String {
        yaml::serialize(&[
            ("started_at", self.started_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            ("full_sha", self.full_sha.clone()),
            ("group", self.group.clone().unwrap_or_default()),
        ])
    }

    fn parse(text: &str) -> Result<Self> {
        let map = yaml::parse(text);
        let started_at_str = map
            .get("started_at")
            .ok_or_else(|| anyhow::anyhow!("lock missing `started_at`"))?;
        let started_at = DateTime::parse_from_rfc3339(started_at_str)
            .with_context(|| format!("invalid started_at: {started_at_str:?}"))?
            .with_timezone(&Utc);
        let full_sha = map
            .get("full_sha")
            .ok_or_else(|| anyhow::anyhow!("lock missing `full_sha`"))?
            .clone();
        let group = map.get("group").filter(|s| !s.is_empty()).cloned();
        Ok(Self {
            started_at,
            full_sha,
            group,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_with_group() {
        let l = Lock::new(
            "abc123def456abc123def456abc123def4567890".into(),
            Some("ios".into()),
        );
        let s = l.serialize();
        let parsed = Lock::parse(&s).unwrap();
        assert_eq!(parsed.full_sha, l.full_sha);
        assert_eq!(parsed.group.as_deref(), Some("ios"));
    }

    #[test]
    fn round_trip_without_group() {
        let l = Lock::new("0".repeat(40), None);
        let s = l.serialize();
        assert!(!s.contains("group:"), "groupless lock leaked group line: {s:?}");
        let parsed = Lock::parse(&s).unwrap();
        assert_eq!(parsed.group, None);
    }

    #[test]
    fn write_then_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("worktree-pool/lock");
        let l = Lock::new("0".repeat(40), Some("ios".into()));
        l.write(&path).unwrap();
        let r = Lock::read(&path).unwrap();
        assert_eq!(r.full_sha, l.full_sha);
        assert_eq!(r.group.as_deref(), Some("ios"));
    }

    #[test]
    fn parse_rejects_missing_required_fields() {
        assert!(Lock::parse("group: ios\n").is_err());
        assert!(Lock::parse("started_at: 2026-01-01T00:00:00Z\n").is_err());
    }
}
