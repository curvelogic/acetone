//! Snapshot corpus for user-facing Cypher diagnostics.
//!
//! Each input is rendered exactly as the CLI surfaces it to a user (parse
//! error via `ParseError::render`, binder error via `BindError::render`) and
//! captured with `insta`. The point is a single reviewable file of every
//! diagnostic, so the 0.1.1 error-message beads land as visible snapshot
//! diffs (`cargo insta review`) rather than silent rewordings.
//!
//! The baseline captures the current output — terse messages and missing
//! suggestions still to be improved by later beads — so that those changes
//! are reviewed diffs, not invisible ones. (The redundant `at bytes …`
//! suffix has been removed in favour of the `line/column` location.)

use acetone_cypher::bind::{BindMode, Catalogue, bind};
use acetone_cypher::parse;

/// Render the diagnostic a user would see for a read query: parse, then bind
/// in lenient mode (the CLI's mode when no schema is declared). Returns the
/// exact rendered string, or a marker when the input unexpectedly succeeds.
fn diagnose(query: &str) -> String {
    let parsed = match parse(query) {
        Ok(p) => p,
        Err(e) => return e.render(query),
    };
    match bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient) {
        Ok(_) => "(*no diagnostic: parses and binds cleanly*)".to_string(),
        Err(e) => e.render(query),
    }
}

/// Inputs whose diagnostics we pin. Grouped by the layer they exercise; the
/// parser set seeds from `corpus.rs`'s INVALID list, the binder set covers the
/// name/scope/aggregation failures reachable without a declared schema.
const DIAGNOSTICS: &[(&str, &str)] = &[
    // --- lexer ---
    ("lex/unterminated-string", "RETURN 'unterminated"),
    ("lex/unterminated-block-comment", "RETURN /* open"),
    // --- parser ---
    ("parse/unclosed-node-pattern", "MATCH (n:Host RETURN n"),
    (
        "parse/expression-expected",
        "MATCH (n:Host) WHERE n.x > RETURN n",
    ),
    ("parse/unknown-leading-keyword", "MATCHX (n) RETURN n"),
    ("parse/incomplete-match", "MATCH (n)"),
    ("parse/bare-return", "RETURN"),
    ("parse/double-return", "MATCH (n) RETURN n RETURN n"),
    ("parse/where-without-predicate", "MATCH (n) WHERE RETURN n"),
    ("parse/incomplete-return-expr", "MATCH (n) RETURN"),
    // --- binder ---
    ("bind/undefined-variable", "MATCH (n) RETURN m"),
    (
        "bind/undefined-in-where",
        "MATCH (n) WHERE m.x > 1 RETURN n",
    ),
    (
        "bind/order-by-undefined",
        "MATCH (n) RETURN n.x ORDER BY nope",
    ),
];

#[test]
fn cypher_diagnostics_snapshot() {
    let mut out = String::new();
    for (label, query) in DIAGNOSTICS {
        out.push_str("### ");
        out.push_str(label);
        out.push('\n');
        out.push_str("input:  ");
        out.push_str(query);
        out.push('\n');
        out.push_str("error:  ");
        out.push_str(&diagnose(query));
        out.push_str("\n\n");
    }
    insta::assert_snapshot!("cypher_diagnostics", out);
}
