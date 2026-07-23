//! Shared machine-readable (`--json`) output for the read commands.
//!
//! **Stability:** the JSON *shape* emitted here is deliberately NOT a
//! stability commitment pre-1.0 and may change at any minor release, with the
//! change noted in the CHANGELOG (acetone-lk1: the CLI is its own product
//! surface, spec §7, and is not covered by the ADR-0046 library freeze —
//! see `STABILITY.md`). Pin your acetone version if you script against exact
//! field names or nesting.
//!
//! All emission goes through `serde_json`, so string escaping is correct: it
//! handles quotes, backslashes and the C0 controls (`< 0x20`). But
//! `serde_json` leaves DEL (`0x7f`), the C1 range (`0x80..=0x9f`) and the
//! bidirectional formatting overrides (Trojan source) raw, and
//! graph property values, labels and commit messages are attacker-writable
//! (a hostile clone). To meet the same terminal-safety bar the human paths
//! meet with `sanitise_line` — and that the query engine's JSON path meets in
//! `json_string` (Phase 2 security review MINOR-1; bidi added in 7bn.19) —
//! [`emit_json`] escapes those remaining characters to `\u…` on the way out. The escapes
//! round-trip: a JSON parser reads `` back to the original byte.

use acetone_core::model::Value;
use serde_json::{Value as Json, json};

use crate::output::outln;

/// Print a JSON value as pretty-printed text (one document, trailing
/// newline), through the pipe-safe `outln!` macro.
pub fn emit_json(value: &Json) {
    // Our values are built from finite data and always serialise; if that
    // ever failed we would rather emit `null` than panic a pipeline.
    match serde_json::to_string_pretty(value) {
        Ok(text) => outln!("{}", escape_residual_controls(&text)),
        Err(_) => outln!("null"),
    }
}

/// Whether `serde_json`'s serialiser leaves this character raw where it is
/// unsafe to reach the terminal: DEL (`0x7f`), the C1 range (`0x80..=0x9f`),
/// and the bidirectional formatting overrides (Trojan source). The C0 controls
/// (`< 0x20`) `serde_json` already escapes inside strings, and it never emits a
/// raw C0 in structure except the pretty-printer's layout newlines — which we
/// must keep — so this deliberately excludes them. The bidi characters never
/// occur in JSON structure either, only in string content, so replacing them
/// document-wide is safe.
fn is_residual_unsafe(c: char) -> bool {
    c == '\u{7f}' || ('\u{80}'..='\u{9f}').contains(&c) || crate::value::is_bidi_control(c)
}

/// Escape the characters `serde_json` leaves raw yet unsafe for the terminal
/// ([`is_residual_unsafe`]) as `\uXXXX`. Those characters never occur in JSON
/// structure, so replacing them across the whole document is safe, keeps it
/// valid, and round-trips (a parser reads `` back to the original character).
/// The pretty-printer's structural newlines and indentation are untouched.
fn escape_residual_controls(text: &str) -> String {
    if !text.chars().any(is_residual_unsafe) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_residual_unsafe(c) {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Lower-case hex for an opaque byte string.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Convert a graph [`Value`] to a JSON value.
///
/// Scalars map to their natural JSON forms (string→string, int→number,
/// bool→bool, null→null). Non-finite floats have no JSON scalar, so they
/// render as their string form (`"NaN"`, `"inf"`), mirroring the query
/// engine's JSON path. Value kinds with no clean JSON scalar — bytes and the
/// temporal types — become a tagged object `{ "$type": "…", … }` carrying
/// their raw components, so they round-trip losslessly and are never
/// ambiguous with a plain string.
pub fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            // No JSON number for NaN/±Infinity: fall back to the string form.
            .unwrap_or_else(|| Json::String(f.to_string())),
        Value::String(s) => Json::String(s.clone()),
        Value::Bytes(b) => json!({ "$type": "bytes", "hex": hex(b) }),
        Value::Date(d) => json!({ "$type": "date", "days": d.days }),
        Value::Time(t) => json!({ "$type": "time", "nanos": t.nanos }),
        Value::DateTime(dt) => json!({
            "$type": "datetime",
            "epoch_nanos": dt.epoch_nanos,
            "offset_minutes": dt.offset_minutes,
        }),
        Value::Duration(d) => json!({
            "$type": "duration",
            "months": d.months,
            "days": d.days,
            "nanos": d.nanos,
        }),
        Value::List(items) => Json::Array(items.iter().map(value_to_json).collect()),
    }
}

/// A key tuple as a JSON array of its element values.
pub fn key_tuple_to_json(key: &[Value]) -> Json {
    Json::Array(key.iter().map(value_to_json).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residual_pass_escapes_c1_del_and_bidi_but_keeps_layout() {
        // serde_json leaves DEL, the C1 range and the bidirectional overrides
        // raw in string content; the residual pass must escape all three while
        // preserving the pretty-printer's structural newlines and indentation.
        let doc = serde_json::to_string_pretty(&json!({
            "subject": "safe\u{202e}reversed\u{202c}\u{7f}\u{85}",
        }))
        .unwrap();
        let escaped = escape_residual_controls(&doc);

        // None of the dangerous characters survive raw.
        for c in ['\u{202e}', '\u{202c}', '\u{7f}', '\u{85}'] {
            assert!(!escaped.contains(c), "{c:?} leaked raw");
        }
        // Their escaped forms are present, and the result still parses.
        assert!(escaped.contains("\\u202e"));
        assert!(escaped.contains("\\u007f"));
        // Structural layout (newline + indentation) is untouched.
        assert!(escaped.contains("\n  \"subject\""));
        let reparsed: Json = serde_json::from_str(&escaped).unwrap();
        assert_eq!(
            reparsed["subject"],
            json!("safe\u{202e}reversed\u{202c}\u{7f}\u{85}")
        );
    }

    #[test]
    fn clean_document_is_returned_unchanged() {
        let doc = serde_json::to_string_pretty(&json!({ "os": "linux 👩‍👧" })).unwrap();
        assert_eq!(escape_residual_controls(&doc), doc);
    }
}
