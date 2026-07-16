//! Property-value parsing and display for the plumbing commands.
//!
//! Phase 1 has no Cypher front end, so `--prop`/key arguments are typed by
//! a single, documented heuristic rather than a real literal grammar:
//! **parse as [`acetone_core::model::Value::Int`] if the whole argument is a
//! valid `i64`, otherwise take it verbatim as [`acetone_core::model::Value::String`]**.
//! There is no way to force a numeric-looking string to stay a string, or
//! to write any other value kind (bool, float, list, temporal) from the
//! command line — that waits for Cypher literals.

use acetone_core::model::Value;
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

/// Bidirectional / directional-formatting control characters that reorder how
/// the surrounding text is displayed without being [`char::is_control`]
/// (Unicode "Trojan Source", CVE-2021-42574). A hostile-clone commit subject,
/// property value or branch name could otherwise rearrange what the terminal
/// shows, so a line reads differently from the bytes it holds. These have no
/// legitimate use in visible content, so they are escaped everywhere
/// repository-controlled text reaches the terminal — matching the identifier
/// renderer, whose `{:?}` already escapes them.
///
/// Scope is deliberately the *directional* set only, not zero-width or other
/// format (Cf) characters: those enable invisibility/homoglyph confusion (a
/// lesser, non-reordering concern) and escaping them would mangle legitimate
/// data such as emoji zero-width-joiner sequences in property values.
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c,
        '\u{061C}'                     // ARABIC LETTER MARK
        | '\u{200E}' | '\u{200F}'      // LEFT-TO-RIGHT / RIGHT-TO-LEFT MARK
        | '\u{202A}'..='\u{202E}'      // LRE, RLE, PDF, LRO, RLO
        | '\u{2066}'..='\u{2069}'      // LRI, RLI, FSI, PDI
    )
}

/// Whether a character must be escaped before it reaches the terminal in
/// repository-controlled text: the C0/C1/DEL controls ([`char::is_control`])
/// plus the bidirectional formatting characters ([`is_bidi_control`]).
pub(crate) fn is_unsafe_for_display(c: char) -> bool {
    c.is_control() || is_bidi_control(c)
}

/// Neutralise dangerous characters in a repository-controlled line of text
/// destined for the terminal, leaving everything printable untouched.
///
/// Unlike [`format_label`]'s `{:?}` (right for identifier-shaped strings,
/// where the quotes aid reading), this is for sentence- or line-shaped
/// output — commit subjects, trailers, fsck findings — where quoting the
/// whole line would hurt readability but ANSI/C1 sequences and bidirectional
/// overrides from a hostile clone must never reach the terminal raw (see
/// [`is_unsafe_for_display`]).
pub fn sanitise_line(s: &str) -> String {
    if s.chars().any(is_unsafe_for_display) {
        s.chars()
            .map(|c| {
                if is_unsafe_for_display(c) {
                    c.escape_default().to_string()
                } else {
                    c.to_string()
                }
            })
            .collect()
    } else {
        s.to_owned()
    }
}

/// Render a label, relationship type or other identifier-shaped string for
/// output. Thin re-export of the canonical renderer in [`acetone_model`] so
/// the CLI and every layer's error messages format identifiers identically
/// (and escape attacker-writable control characters the same way).
pub use acetone_core::model::display::format_label;

/// Render a value for human-readable output (`get-node`, `list-nodes`). Thin
/// re-export of the canonical renderer in [`acetone_model`], shared with the
/// error-message paths so a value never renders two different ways.
pub use acetone_core::model::display::format_value;

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

    #[test]
    fn format_label_escapes_control_characters() {
        assert_eq!(format_label("Person"), "\"Person\"");
        assert_eq!(format_label("a\x1b[31mb"), "\"a\\u{1b}[31mb\"");
        assert_eq!(format_label("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn sanitise_line_neutralises_control_characters() {
        // Printable text, including unicode, quotes and emoji, passes
        // untouched — even emoji built from zero-width-joiner sequences,
        // which are deliberately out of the directional-control scope.
        assert_eq!(
            sanitise_line("add web3 (\"fast\") — déjà vu 👩‍👧"),
            "add web3 (\"fast\") — déjà vu 👩‍👧"
        );
        // ANSI escape, bell, carriage return: escaped, never raw.
        let hostile = "ok\x1b[8m hidden\x07\rspoof";
        let clean = sanitise_line(hostile);
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x07'));
        assert!(!clean.contains('\r'));
        assert_eq!(
            clean,
            "ok\\u{{1b}}[8m hidden\\u{{7}}\\rspoof"
                .replace("{{", "{")
                .replace("}}", "}")
        );
    }

    #[test]
    fn sanitise_line_neutralises_bidi_overrides() {
        // A right-to-left override (U+202E) reorders following text — the
        // Trojan-source trick. It is not `char::is_control()`, so it must be
        // caught by the bidi guard, not the control guard.
        assert!(!'\u{202e}'.is_control());
        let hostile = "safe\u{202e}txet_desrever\u{202c}";
        let clean = sanitise_line(hostile);
        assert!(!clean.contains('\u{202e}'));
        assert!(!clean.contains('\u{202c}'));
        assert_eq!(clean, "safe\\u{202e}txet_desrever\\u{202c}");
    }
}
