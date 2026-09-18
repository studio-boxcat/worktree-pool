//! The stdout contract. Subcommands build an [`Outcome`] and return it; `main`
//! is the only writer and the only place that decides an exit code.
//! See [[../docs/cli.md#output-contract]].
use serde_json::Value;
use std::path::Path;

/// A subcommand's stdout payload.
pub enum Output {
    /// Stdout stays empty — the verb reports progress on stderr only.
    None,
    /// Exactly one bare line. Reserved for slot paths, which callers consume as
    /// `$(worktree-pool … acquire …)` without a JSON parser in the loop.
    Line(String),
    /// A structured report: compact JSON, one line, `| jq` to read.
    Json(Value),
}

/// Nonzero exit despite a well-formed run.
pub enum Failure {
    /// Exit code only, no stderr — the code *is* the answer (`path`: "no held
    /// slot with this lease"), so callers can branch on it without filtering noise.
    Silent(i32),
    /// `main` prints the anyhow chain to stderr and derives the code from it.
    Reported(anyhow::Error),
}

/// Payload plus exit disposition. These are independent: `doctor` and
/// `validate-gitmodules` emit a full report *and* exit nonzero when it contains
/// problems — the report is the point, so it must survive the failure.
pub struct Outcome {
    pub output: Output,
    pub failure: Option<Failure>,
}

impl Outcome {
    pub fn none() -> Self {
        Output::None.into()
    }
    pub fn line(s: impl Into<String>) -> Self {
        Output::Line(s.into()).into()
    }
    pub fn json(v: Value) -> Self {
        Output::Json(v).into()
    }
    /// A report that also fails, e.g. a health check that found errors.
    pub fn json_failing(v: Value, e: anyhow::Error) -> Self {
        Self {
            output: Output::Json(v),
            failure: Some(Failure::Reported(e)),
        }
    }
    pub fn silent_exit(code: i32) -> Self {
        Self {
            output: Output::None,
            failure: Some(Failure::Silent(code)),
        }
    }
}

impl From<Output> for Outcome {
    fn from(output: Output) -> Self {
        Self {
            output,
            failure: None,
        }
    }
}

/// Paths cross the JSON boundary lossily on non-UTF-8 (`to_string_lossy`);
/// a mangled path in a report beats dropping the field.
pub fn path(p: &Path) -> Value {
    Value::String(p.to_string_lossy().into_owned())
}

/// `None` → JSON `null`. Absent data is null, never a placeholder string.
pub fn opt_str(s: Option<&str>) -> Value {
    s.map_or(Value::Null, |s| Value::String(s.to_string()))
}

/// Non-empty lines as a JSON array — for git's own multi-line text output.
pub fn lines(s: &str) -> Value {
    s.lines().filter(|l| !l.is_empty()).collect()
}
