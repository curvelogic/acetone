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
use acetone_model::schema::{LabelDef, PropertyType, RelTypeDef, SchemaEntry};
use std::collections::BTreeMap;

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

/// A small catalogue for the strict-mode "did you mean" cases: a shaped `Host`
/// label and a `RUNS` relationship type give the label/property/rel-type
/// suggestions something to match against.
fn suggestion_catalogue() -> Catalogue {
    let mut types = BTreeMap::new();
    types.insert("hostname".to_string(), PropertyType::String);
    let label = LabelDef::new(vec!["hostname".to_string()], types, [], []).unwrap();
    Catalogue::from_entries([
        SchemaEntry::Label {
            name: "Host".into(),
            def: label,
        },
        SchemaEntry::RelType {
            name: "RUNS".into(),
            def: RelTypeDef::new(None, BTreeMap::new(), []).unwrap(),
        },
    ])
}

/// As [`diagnose`], but binds in strict mode against [`suggestion_catalogue`],
/// so unknown labels, relationship types and properties are hard errors that
/// carry near-match suggestions.
fn diagnose_strict(query: &str) -> String {
    let parsed = match parse(query) {
        Ok(p) => p,
        Err(e) => return e.render(query),
    };
    match bind(query, &parsed, &suggestion_catalogue(), BindMode::Strict) {
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
    // A bare `[LINK]` is a variable, not a type: CREATE has no type at all.
    ("bind/create-rel-missing-type", "CREATE (:A)-[]->(:B)"),
    // Two types on one created relationship: identity would be ambiguous.
    ("bind/create-rel-multiple-types", "CREATE (:A)-[:X|Y]->(:B)"),
    // Unknown function: a near typo gets a "did you mean", nonsense does not.
    // (Function resolution is schema-independent, so it fires in lenient mode.)
    ("bind/unknown-function-typo", "RETURN toUppr('a')"),
    ("bind/unknown-function-nonsense", "RETURN frobnicate(1)"),
];

/// Strict-mode inputs whose diagnostics we pin: unknown labels, relationship
/// types and properties, each with a near typo (suggested) and nonsense (not).
const STRICT_DIAGNOSTICS: &[(&str, &str)] = &[
    ("bind/unknown-label-typo", "MATCH (n:Hst) RETURN n"),
    ("bind/unknown-label-nonsense", "MATCH (n:Zzzzzz) RETURN n"),
    (
        "bind/unknown-rel-type-typo",
        "MATCH (h:Host)-[:RUNZ]->(x:Host) RETURN h",
    ),
    (
        "bind/unknown-rel-type-nonsense",
        "MATCH (h:Host)-[:ZZZZZZ]->(x:Host) RETURN h",
    ),
    (
        "bind/unknown-property-typo",
        "MATCH (h:Host {hstname: 'a'}) RETURN h",
    ),
    (
        "bind/unknown-property-nonsense",
        "MATCH (h:Host {zzzzzzz: 1}) RETURN h",
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
    for (label, query) in STRICT_DIAGNOSTICS {
        out.push_str("### ");
        out.push_str(label);
        out.push_str(" (strict)\n");
        out.push_str("input:  ");
        out.push_str(query);
        out.push('\n');
        out.push_str("error:  ");
        out.push_str(&diagnose_strict(query));
        out.push_str("\n\n");
    }
    insta::assert_snapshot!("cypher_diagnostics", out);
}
