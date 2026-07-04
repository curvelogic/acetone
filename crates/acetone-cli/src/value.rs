//! Property-value parsing and display for the plumbing commands.
//!
//! Phase 1 has no Cypher front end, so `--prop`/key arguments are typed by
//! a single, documented heuristic rather than a real literal grammar:
//! **parse as [`acetone_model::Value::Int`] if the whole argument is a
//! valid `i64`, otherwise take it verbatim as [`acetone_model::Value::String`]**.
//! There is no way to force a numeric-looking string to stay a string, or
//! to write any other value kind (bool, float, list, temporal) from the
//! command line — that waits for Cypher literals.

use acetone_model::Value;
use anyhow::{Result, bail};

/// Parse one CLI argument as a property/key value using the int-or-string
/// heuristic documented on the module.
pub fn parse_value(raw: &str) -> Value {
    match raw.parse::<i64>() {
        Ok(i) => Value::Int(i),
        Err(_) => Value::String(raw.to_owned()),
    }
}

/// Split a `KEY=VALUE` argument on its first `=`, for `--prop`/`--trailer`.
/// Errors with the offending argument quoted, not a parser Debug dump.
pub fn parse_kv<'a>(raw: &'a str, flag: &str) -> Result<(&'a str, &'a str)> {
    match raw.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key, value)),
        _ => bail!("invalid {flag} argument {raw:?}: expected KEY=VALUE"),
    }
}

/// Render a value for human-readable output (`get-node`, `list-nodes`).
/// Only [`Value::Int`] and [`Value::String`] are reachable from this CLI's
/// own input, but every variant is handled so a record written by another
/// client never panics the display path.
pub fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => format!("{s:?}"),
        Value::Bytes(b) => format!("bytes({} B)", b.len()),
        Value::Date(d) => format!("date({})", d.days),
        Value::Time(t) => format!("time({})", t.nanos),
        Value::DateTime(dt) => format!("datetime({}, {})", dt.epoch_nanos, dt.offset_minutes),
        Value::Duration(d) => format!("duration({}mo {}d {}ns)", d.months, d.days, d.nanos),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", parts.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_like_strings_parse_as_int() {
        assert_eq!(parse_value("42"), Value::Int(42));
        assert_eq!(parse_value("-7"), Value::Int(-7));
        assert_eq!(parse_value("0"), Value::Int(0));
    }

    #[test]
    fn everything_else_is_a_string() {
        assert_eq!(parse_value("bob"), Value::String("bob".into()));
        assert_eq!(parse_value("3.14"), Value::String("3.14".into()));
        assert_eq!(parse_value(""), Value::String("".into()));
        assert_eq!(parse_value("007x"), Value::String("007x".into()));
    }

    #[test]
    fn kv_splits_on_first_equals() {
        assert_eq!(parse_kv("a=b=c", "--prop").unwrap(), ("a", "b=c"));
        assert_eq!(parse_kv("name=Alice", "--prop").unwrap(), ("name", "Alice"));
    }

    #[test]
    fn kv_rejects_missing_equals_or_empty_key() {
        assert!(parse_kv("noequals", "--prop").is_err());
        assert!(parse_kv("=value", "--prop").is_err());
    }
}
