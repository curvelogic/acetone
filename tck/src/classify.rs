//! Scenario classification against the current acetone surface.
//!
//! Until the executor (yzc.5) and binder (yzc.4) exist, classification is
//! parse-only, and its honesty rules are:
//!
//! - We only claim **Passed** for what the parser alone can prove: the
//!   scenario expects `SyntaxError` at compile time and acetone's parser
//!   rejects the query.
//! - We only claim **Failed** for what the parser alone can disprove: a
//!   query the TCK requires to be compile-valid that our parser rejects —
//!   unless the query uses deferred syntax (spec §5.1 deferrals, Phase 3
//!   write clauses), which is Unsupported instead.
//! - Everything else is **Unsupported**, split by the missing capability.

use crate::scenario::{Expectation, ScenarioPlan};
use crate::vocabulary::ErrorPhase;

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Verified: currently always a compile-time rejection the TCK demanded.
    Passed,
    /// Acetone demonstrably disagrees with the TCK.
    Failed {
        reason: String,
    },
    Unsupported(UnsupportedReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedReason {
    /// Needs the executor (yzc.5) — the common case.
    Executor,
    /// Needs the binder (yzc.4) to classify compile-time errors.
    CompileClassification,
    /// Uses syntax acetone defers: spec §5.1 deferrals or Phase 3 write
    /// clauses (acetone-mex.1).
    DeferredSyntax,
}

/// Keyword heuristic for deferred syntax. Token-based (not substring), so
/// property names like `offset` do not trip `SET`; string literals can
/// still fool it, which is acceptable for a reporting bucket. Applied
/// symmetrically: a deferred-syntax parse rejection is never Failed AND
/// never Passed — the parser rejecting a construct wholesale proves
/// nothing about the specific flaw a scenario tests.
fn uses_deferred_syntax(query: &str) -> bool {
    const DEFERRED: &[&str] = &[
        // Level W — Phase 3 (acetone-mex.1).
        "CREATE",
        "MERGE",
        "SET",
        "REMOVE",
        "DELETE",
        "DETACH",
        // spec §5.1 explicit deferrals that are keyword-detectable
        // (FOREACH; shortest-path functions). CALL {} subqueries, map
        // projections and temporal arithmetic are also deferred by §5.1
        // but not detectable by token — their parse failures count as
        // Failed, which errs against acetone.
        "FOREACH",
        "SHORTESTPATH",
        "ALLSHORTESTPATHS",
        // Outside the §5.1 v0.1 target subset (not named in the Level R
        // list): UNION, and EXISTS in both its function and subquery
        // forms (the token check cannot tell them apart, which only
        // matters on parse failure). Recorded on bead acetone-yzc.3 and
        // flagged for the Phase 2 report.
        "UNION",
        "EXISTS",
    ];
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|token| DEFERRED.iter().any(|kw| token.eq_ignore_ascii_case(kw)))
}

pub fn classify(plan: &ScenarioPlan) -> Verdict {
    let Some(query) = &plan.query else {
        // No `When executing query:` step the harness models — nothing to
        // judge without an executor.
        return Verdict::Unsupported(UnsupportedReason::Executor);
    };
    let parse_result = acetone_cypher::parse(query);

    match &plan.expectation {
        Expectation::Error {
            error_type,
            phase: ErrorPhase::CompileTime,
            ..
        } => {
            match (parse_result.is_err(), error_type.as_str()) {
                // The parser rejects the construct wholesale, so the
                // rejection says nothing about the flaw under test —
                // crediting it would inflate the pass rate with passes
                // that evaporate when the deferred syntax lands.
                (true, _) if uses_deferred_syntax(query) => {
                    Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
                }
                // The TCK demanded a compile-time SyntaxError and the
                // parser delivered one. (The rejection *reason* is not
                // verified — parse-only classification cannot know it.)
                (true, "SyntaxError") => Verdict::Passed,
                // Rejected at compile time, but the TCK wants a different
                // error class — the binder must classify.
                (true, _) => Verdict::Unsupported(UnsupportedReason::CompileClassification),
                // Parses cleanly; the compile-time error the TCK expects
                // would come from the binder (aggregation misuse, unbound
                // variables and so on are SyntaxError in TCK terms but
                // post-parse in any real front end).
                (false, _) => Verdict::Unsupported(UnsupportedReason::CompileClassification),
            }
        }
        Expectation::Error {
            phase: ErrorPhase::Runtime,
            ..
        } => match parse_result {
            // Runtime-error scenarios are compile-valid by definition.
            Err(e) if !uses_deferred_syntax(query) => Verdict::Failed {
                reason: format!("TCK requires this query to compile, parser rejected it: {e}"),
            },
            Err(_) => Verdict::Unsupported(UnsupportedReason::DeferredSyntax),
            Ok(_) => Verdict::Unsupported(UnsupportedReason::Executor),
        },
        Expectation::Error {
            phase: ErrorPhase::AnyTime,
            ..
        } => match parse_result {
            // "Any time" allows compile-time rejection, but the TCK still
            // pins the error class, which the parser cannot claim alone.
            Err(_) => Verdict::Unsupported(UnsupportedReason::CompileClassification),
            Ok(_) => Verdict::Unsupported(UnsupportedReason::Executor),
        },
        Expectation::Rows | Expectation::EmptyResult | Expectation::None => match parse_result {
            Err(e) if !uses_deferred_syntax(query) => Verdict::Failed {
                reason: format!("TCK requires this query to be valid, parser rejected it: {e}"),
            },
            Err(_) => Verdict::Unsupported(UnsupportedReason::DeferredSyntax),
            Ok(_) => Verdict::Unsupported(UnsupportedReason::Executor),
        },
    }
}

/// Parse statistics over the corpus's queries-under-test, independent of
/// scenario verdicts: how much of the TCK's query surface the parser
/// accepts today. Setup queries are excluded (they are Level W by
/// construction).
#[derive(Debug, Default, serde::Serialize)]
pub struct ParseStats {
    pub queries: usize,
    pub parse_ok: usize,
    pub parse_err_deferred: usize,
    pub parse_err_other: usize,
}

pub fn parse_stats(plans: &[ScenarioPlan]) -> ParseStats {
    let mut stats = ParseStats::default();
    for plan in plans {
        let Some(query) = &plan.query else { continue };
        stats.queries += 1;
        match acetone_cypher::parse(query) {
            Ok(_) => stats.parse_ok += 1,
            Err(_) if uses_deferred_syntax(query) => stats.parse_err_deferred += 1,
            Err(_) => stats.parse_err_other += 1,
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Expectation;

    fn plan(query: &str, expectation: Expectation) -> ScenarioPlan {
        ScenarioPlan {
            feature_path: "test.feature".into(),
            scenario_name: "test".into(),
            setup_queries: vec![],
            named_graph: None,
            has_parameters: false,
            needs_procedures: false,
            query: Some(query.into()),
            expectation,
        }
    }

    fn compile_error(error_type: &str) -> Expectation {
        Expectation::Error {
            error_type: error_type.into(),
            phase: ErrorPhase::CompileTime,
            detail: "whatever".into(),
        }
    }

    #[test]
    fn parser_rejection_passes_expected_syntax_error() {
        let verdict = classify(&plan(
            "MATCH (n:Host RETURN n",
            compile_error("SyntaxError"),
        ));
        assert_eq!(verdict, Verdict::Passed);
    }

    #[test]
    fn clean_parse_of_expected_compile_error_is_binder_work() {
        let verdict = classify(&plan("MATCH (n) RETURN m", compile_error("SyntaxError")));
        assert_eq!(
            verdict,
            Verdict::Unsupported(UnsupportedReason::CompileClassification)
        );
    }

    #[test]
    fn valid_query_that_fails_to_parse_is_a_failure() {
        // A read query the TCK expects to succeed; if the parser rejected
        // it, that must surface as Failed, not vanish into Unsupported.
        let verdict = classify(&plan("MATCH (n) RETURN n", Expectation::Rows));
        assert_eq!(verdict, Verdict::Unsupported(UnsupportedReason::Executor));

        let verdict = classify(&plan("MATCH (n) RETURN n AS", Expectation::Rows));
        assert!(matches!(verdict, Verdict::Failed { .. }));
    }

    #[test]
    fn deferred_syntax_is_unsupported_not_failed() {
        let verdict = classify(&plan("CREATE (n) RETURN n", Expectation::Rows));
        assert_eq!(
            verdict,
            Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
        );
        let verdict = classify(&plan("FOREACH (x IN [1] | SET n.p = x)", Expectation::Rows));
        assert_eq!(
            verdict,
            Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
        );
    }

    #[test]
    fn deferred_keyword_detection_is_token_based() {
        assert!(!uses_deferred_syntax(
            "MATCH (n) RETURN n.offset, n.created"
        ));
        assert!(uses_deferred_syntax("MATCH (n) SET n.p = 1"));
    }
}
