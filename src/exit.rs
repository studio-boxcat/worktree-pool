//! Distinct exit codes for retry-aware callers. `bail_exit!(kind, "msg")`
//! tags the anyhow error with an `ExitKind`; `main` downcasts the chain and
//! exits with the kind's code (1 otherwise). See [[../docs/cli.md#exit-codes]].
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Contended = 3,
    Capacity = 4,
    UniqueSha = 5,
}

impl ExitKind {
    pub fn code(self) -> i32 {
        self as i32
    }
}

impl fmt::Display for ExitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contended => f.write_str("contended"),
            Self::Capacity => f.write_str("capacity"),
            Self::UniqueSha => f.write_str("unique-sha"),
        }
    }
}

impl std::error::Error for ExitKind {}

/// Like `anyhow::bail!` but tags the error with an `ExitKind` so `main` can
/// exit with a distinct code.
#[macro_export]
macro_rules! bail_exit {
    ($kind:expr, $($arg:tt)*) => {
        return Err(::anyhow::Error::new($kind).context(format!($($arg)*)))
    };
}
