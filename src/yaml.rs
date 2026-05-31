//! Line-oriented YAML: top-level scalar key-value pairs only. No nesting,
//! no flow style, no quoted values. The contract is mirrored by meow-tower's
//! `wt-cleanup.sh` greps; format must stay this strict.
use std::collections::BTreeMap;

/// Serialize an ordered list of `(key, value)` pairs. Skips entries with empty values.
pub fn serialize(pairs: &[(&str, String)]) -> String {
    let mut out = String::new();
    for (k, v) in pairs {
        if v.is_empty() {
            continue;
        }
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push('\n');
    }
    out
}

/// Parse line-oriented YAML. Tolerates blank lines and `#`-comments. Skips
/// malformed lines silently — caller decides which keys are required.
pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{FEFF}'); // strip BOM
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(idx) = line.find(':') else {
            continue;
        };
        let k = line[..idx].trim();
        let v = line[idx + 1..].trim();
        if k.is_empty() {
            continue;
        }
        out.insert(k.to_string(), v.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    // `benches/yaml.rs` pulls this file in via `#[path]`; in that build the
    // `#[test]` bodies are stripped but this `use` survives, so it reads as
    // unused there (it is used in the normal `cargo test` build). Allow it
    // rather than split the tests out for a bench-only lint.
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn round_trip_scalar() {
        let pairs = vec![("kind", "build".to_string()), ("name", "abc".to_string())];
        let text = serialize(&pairs);
        let parsed = parse(&text);
        assert_eq!(parsed.get("kind").map(String::as_str), Some("build"));
        assert_eq!(parsed.get("name").map(String::as_str), Some("abc"));
    }

    #[test]
    fn skips_empty_values() {
        let pairs = vec![("kind", "build".to_string()), ("group", String::new())];
        let text = serialize(&pairs);
        assert!(text.contains("kind: build"));
        assert!(!text.contains("group:"));
    }

    #[test]
    fn line_anchored_volatile_grep_contract() {
        // wt-cleanup.sh greps `^volatile: true` line-anchored. Format must be exact.
        let pairs = vec![("volatile", "true".to_string())];
        let text = serialize(&pairs);
        assert!(
            text.lines().any(|l| l == "volatile: true"),
            "no line-anchored 'volatile: true' in {text:?}"
        );
    }

    #[test]
    fn parse_tolerates_comments_and_blanks() {
        let parsed = parse(
            "
# header
kind: build

name: foo
",
        );
        assert_eq!(parsed.get("kind").map(String::as_str), Some("build"));
        assert_eq!(parsed.get("name").map(String::as_str), Some("foo"));
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn parse_strips_bom_and_crlf() {
        let parsed = parse("\u{FEFF}kind: build\r\nname: foo\r\n");
        assert_eq!(parsed.get("kind").map(String::as_str), Some("build"));
        assert_eq!(parsed.get("name").map(String::as_str), Some("foo"));
    }
}
