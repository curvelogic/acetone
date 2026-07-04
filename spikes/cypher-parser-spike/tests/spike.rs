//! Keeps the spike's evidence honest: every valid query in the
//! representative set must parse with the hand-rolled parser, every
//! invalid one must be rejected with a positioned error, and the AT
//! extension must surface in the AST.

use cypher_parser_spike::handrolled::{self, ast};
use cypher_parser_spike::queries::{Category, QUERIES};

#[test]
fn handrolled_parses_every_valid_query() {
    for q in QUERIES.iter().filter(|q| q.category != Category::Invalid) {
        if let Err(e) = handrolled::parse(q.text) {
            panic!("{} failed to parse: {e}", q.name);
        }
    }
}

#[test]
fn handrolled_rejects_every_invalid_query_without_panicking() {
    for q in QUERIES.iter().filter(|q| q.category == Category::Invalid) {
        let err = handrolled::parse(q.text).expect_err(q.name);
        assert!(
            err.span.end <= q.text.len(),
            "{}: error span out of bounds",
            q.name
        );
    }
}

#[test]
fn at_ref_lands_in_the_ast() {
    let query = handrolled::parse("MATCH (n:Host) AT 'main~5' RETURN n").unwrap();
    match &query.clauses[0] {
        ast::Clause::Match {
            at_ref: Some((refspec, _)),
            ..
        } => {
            assert_eq!(refspec, "main~5");
        }
        other => panic!("expected MATCH with AT ref, got {other:?}"),
    }
}

#[test]
fn decypher_current_coverage_snapshot() {
    // Documents decypher 0.2.0-alpha.6's gaps against the set at spike
    // time; if this starts failing, the crate has improved and the Gate B
    // trade-off deserves a second look.
    let expected_failures = [
        "list-comprehension-map-literal",
        "pattern-predicate",
        "registry-orphaned-software",
        "at-ref-time-travel",
        "at-ref-with-where",
    ];
    for q in QUERIES.iter().filter(|q| q.category != Category::Invalid) {
        let outcome = decypher::parse(q.text);
        if expected_failures.contains(&q.name) {
            assert!(outcome.is_err(), "{} now parses under decypher", q.name);
        } else {
            assert!(outcome.is_ok(), "{} regressed under decypher", q.name);
        }
    }
}
