//! Domain-key newtypes — so a slot id, a branch name, a group, and a full
//! commit SHA can't be passed for one another across module boundaries. Thin
//! wrappers over `String`; they cross the stringly-typed `git` shell-out layer
//! via `.as_str()`. No `Deref` on purpose — that would re-open the `&str`
//! coercion these exist to close.
use anyhow::{Result, bail};
use std::fmt;

macro_rules! str_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str { &self.0 }
            #[allow(dead_code)] // not every newtype consumes into its String
            pub fn into_string(self) -> String { self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_string()) }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }
    };
}

str_newtype!(
    /// Canonical slot id: `{group}-{N}` (grouped) or `slot-{N}` (groupless).
    SlotId
);
str_newtype!(
    /// A feature branch — the caller-given acquire `NAME`, held in the slot's branch ref.
    BranchName
);
str_newtype!(
    /// A pool group (e.g. `ios`, `android`). Persisted in `config.yaml`'s `groups`.
    GroupName
);

/// A full 40-hex-char git commit SHA (`git rev-parse`), used for same-SHA
/// exclusion. Validated at construction so `short()` can't panic.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FullSha(String);

impl FullSha {
    pub fn parse(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(s))
        } else {
            bail!("not a full 40-char hex SHA: {s:?}");
        }
    }
    pub fn as_str(&self) -> &str { &self.0 }
    /// 8-char display prefix — never panics (length validated at construction).
    pub fn short(&self) -> &str { &self.0[..8] }
}
impl fmt::Display for FullSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}
impl AsRef<str> for FullSha {
    fn as_ref(&self) -> &str { &self.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_sha_validates_and_shortens() {
        let sha = "a".repeat(40);
        let fs = FullSha::parse(sha.clone()).unwrap();
        assert_eq!(fs.short(), "aaaaaaaa");
        assert_eq!(fs.as_str(), sha);
        assert!(FullSha::parse("abc").is_err(), "short string must be rejected");
        assert!(FullSha::parse("z".repeat(40)).is_err(), "non-hex must be rejected");
    }

    #[test]
    fn str_newtypes_roundtrip() {
        assert_eq!(SlotId::from("ios-0").as_str(), "ios-0");
        assert_eq!(GroupName::from("ios").to_string(), "ios");
        assert_eq!(BranchName::from("feat-x".to_string()).into_string(), "feat-x");
    }
}
