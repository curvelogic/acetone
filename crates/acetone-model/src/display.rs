//! Human-readable rendering of values, labels and node keys for error
//! messages and CLI output.
//!
//! Graph data is attacker-writable and otherwise reaches the terminal
//! verbatim, so string rendering here escapes control characters and ANSI
//! sequences (via `{:?}`) rather than emitting them raw. This is the single
//! canonical renderer: the CLI's human output and every layer's error
//! messages share it, so a node key never leaks Rust `Debug` internals
//! (`[String("web-01")]`) to a user.

use crate::Value;
use crate::graph_keys::NodeKey;

/// Render a value for human-readable output.
///
/// [`Value::String`] is escaped with `{:?}` so control characters and ANSI
/// escapes from a hostile clone are neutralised, never reaching the terminal
/// raw. Every variant is handled so a record written by another client can
/// never panic the display path.
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

/// Render a label, relationship type or other identifier-shaped string,
/// escaping it the same way [`format_value`] escapes [`Value::String`] so
/// attacker-writable identifiers cannot inject terminal control sequences.
pub fn format_label(s: &str) -> String {
    format!("{s:?}")
}

/// Render a list of labels as `["A", "B"]`, each escaped like [`format_label`]
/// so attacker-writable labels cannot inject terminal control sequences.
pub fn format_labels(labels: &[String]) -> String {
    let parts: Vec<String> = labels.iter().map(|l| format_label(l)).collect();
    format!("[{}]", parts.join(", "))
}

/// Render a key tuple as `[a, b]`, each element via [`format_value`].
pub fn format_key_tuple(key: &[Value]) -> String {
    let parts: Vec<String> = key.iter().map(format_value).collect();
    format!("[{}]", parts.join(", "))
}

/// Render a full node identity — label plus key tuple — as `"Label" [a, b]`.
pub fn format_node_key(key: &NodeKey) -> String {
    format_node_identity(key.label(), key.key())
}

/// Render a node identity from a label and key tuple held separately (error
/// sites that carry the two apart rather than a whole [`NodeKey`]).
pub fn format_node_identity(label: &str, key: &[Value]) -> String {
    format!("{} {}", format_label(label), format_key_tuple(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_keys::NodeKey;

    #[test]
    fn strings_are_escaped_not_leaked() {
        // The core defect this renderer exists to prevent: no Debug wrapper.
        assert_eq!(format_value(&Value::String("web-01".into())), "\"web-01\"");
        assert_eq!(format_value(&Value::Int(42)), "42");
        assert_eq!(format_value(&Value::Bool(true)), "true");
        assert_eq!(format_value(&Value::Null), "null");
    }

    #[test]
    fn control_characters_and_ansi_are_neutralised() {
        // A hostile string value must never reach the terminal raw.
        let rendered = format_value(&Value::String("a\x1b[31mb\nc".into()));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\n'));
        assert_eq!(rendered, "\"a\\u{1b}[31mb\\nc\"");
        let label = format_label("evil\x1b[8m");
        assert!(!label.contains('\x1b'));
    }

    #[test]
    fn key_tuple_renders_without_debug_internals() {
        let key = [Value::String("web-01".into()), Value::Int(3)];
        assert_eq!(format_key_tuple(&key), "[\"web-01\", 3]");
        assert_eq!(format_key_tuple(&[]), "[]");
    }

    #[test]
    fn labels_render_escaped_and_bracketed() {
        assert_eq!(
            format_labels(&["Topic".to_string(), "Draft".to_string()]),
            "[\"Topic\", \"Draft\"]"
        );
        assert_eq!(format_labels(&[]), "[]");
        assert_eq!(format_labels(&["Solo".to_string()]), "[\"Solo\"]");
        // A control-char label must be escaped, never reaching the terminal raw.
        let rendered = format_labels(&["evil\x1b[8m".to_string()]);
        assert!(!rendered.contains('\x1b'));
        assert_eq!(rendered, "[\"evil\\u{1b}[8m\"]");
    }

    #[test]
    fn node_identity_pairs_label_and_key() {
        assert_eq!(
            format_node_identity("Host", &[Value::String("web-01".into())]),
            "\"Host\" [\"web-01\"]"
        );
    }

    #[test]
    fn node_key_renders_via_identity() {
        let nk = NodeKey::new("Host", vec![Value::String("web-01".into())]).unwrap();
        assert_eq!(format_node_key(&nk), "\"Host\" [\"web-01\"]");
    }

    #[test]
    fn every_value_variant_renders_without_panic() {
        // A record written by another client must never panic the display.
        for v in [
            Value::Null,
            Value::Bool(false),
            Value::Int(-1),
            Value::Float(1.5),
            Value::String("x".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::List(vec![Value::Int(1), Value::String("y".into())]),
        ] {
            let _ = format_value(&v);
        }
    }
}
