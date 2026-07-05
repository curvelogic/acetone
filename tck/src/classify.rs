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

use acetone_cypher::bind::{BindError, BindMode, Catalogue, bind};
use acetone_cypher::exec::{self, EmptyGraph, ExecError};

use crate::expected;
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
    /// The front end rejects at compile time, but the TCK's expected
    /// error class/detail could not be verified against the rejection.
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

/// Functions outside the v0.1 subset whose absence is deliberate:
/// temporal constructors (spec §5.1 defers "full temporal arithmetic"),
/// spatial (not in scope), and aggregates beyond the §5.1 list (count,
/// sum, avg, min, max, collect). An UnknownFunction rejection naming one
/// of these is deferral, not a defect.
const DEFERRED_FUNCTIONS: &[&str] = &[
    "date",
    "datetime",
    "localdatetime",
    "localtime",
    "time",
    "duration",
    "point",
    "distance",
    "percentileCont",
    "percentileDisc",
    "stDev",
    "stDevP",
    // Nondeterministic; not in the §5.1 subset.
    "rand",
];

fn is_deferred_bind_error(error: &BindError) -> bool {
    match error {
        BindError::UnknownFunction { name, .. } => {
            // Namespaced temporal functions (`datetime.truncate`,
            // `duration.between`) defer with their namespace.
            let head = name.split('.').next().unwrap_or(name);
            DEFERRED_FUNCTIONS
                .iter()
                .any(|f| f.eq_ignore_ascii_case(head))
        }
        _ => false,
    }
}

pub fn classify(plan: &ScenarioPlan) -> Verdict {
    let Some(query) = &plan.query else {
        // No `When executing query:` step the harness models — nothing to
        // judge without an executor.
        return Verdict::Unsupported(UnsupportedReason::Executor);
    };
    // The front end available today: parse, then bind leniently against
    // an empty catalogue (TCK graphs are schema-free).
    let front_end = match acetone_cypher::parse(query) {
        Err(e) => FrontEnd::ParseRejected(e),
        Ok(parsed) => match bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient) {
            // Scenarios that register their own stub procedures need
            // executor-side procedure infrastructure; the binder refusing
            // the unregistered name is not a defect.
            Err(BindError::ProcedureNotFound { .. }) if plan.needs_procedures => {
                return Verdict::Unsupported(UnsupportedReason::Executor);
            }
            Err(e) => FrontEnd::BindRejected(e),
            Ok(_) => FrontEnd::Accepted,
        },
    };

    match &plan.expectation {
        Expectation::Error {
            error_type,
            phase: ErrorPhase::CompileTime,
            detail,
        } => match &front_end {
            // The parser or binder rejects a deferred construct
            // wholesale, so the rejection says nothing about the flaw
            // under test — crediting it would inflate the pass rate with
            // passes that evaporate when the deferred syntax lands.
            FrontEnd::ParseRejected(_) if uses_deferred_syntax(query) => {
                Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
            }
            FrontEnd::BindRejected(e)
                if uses_deferred_syntax(query) || is_deferred_bind_error(e) =>
            {
                Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
            }
            // The TCK demanded a compile-time SyntaxError and the parser
            // delivered one. (The rejection *reason* is not verified —
            // the parser cannot know it.)
            FrontEnd::ParseRejected(_) if error_type == "SyntaxError" => Verdict::Passed,
            FrontEnd::ParseRejected(_) => {
                Verdict::Unsupported(UnsupportedReason::CompileClassification)
            }
            // The binder rejected: Passed only on a detail-verified match
            // (SyntaxError covers post-parse compile errors in TCK
            // vocabulary; ProcedureError maps to ProcedureNotFound).
            FrontEnd::BindRejected(e) => {
                let class_matches = match error_type.as_str() {
                    "SyntaxError" => !matches!(e, BindError::ProcedureNotFound { .. }),
                    "ProcedureError" => matches!(e, BindError::ProcedureNotFound { .. }),
                    _ => false,
                };
                match e.tck_detail() {
                    Some(our_detail) if class_matches && our_detail == detail => Verdict::Passed,
                    _ => Verdict::Unsupported(UnsupportedReason::CompileClassification),
                }
            }
            // Accepted cleanly; the compile-time error the TCK expects is
            // beyond the current front end (type checks, semantics the
            // binder does not model).
            FrontEnd::Accepted => Verdict::Unsupported(UnsupportedReason::CompileClassification),
        },
        Expectation::Error {
            phase: ErrorPhase::Runtime,
            ..
        } => match &front_end {
            // Runtime-error scenarios are compile-valid by definition: a
            // front-end rejection is a defect unless the syntax is
            // deferred.
            FrontEnd::ParseRejected(e) if !uses_deferred_syntax(query) => Verdict::Failed {
                reason: format!("TCK requires this query to compile, parser rejected it: {e}"),
            },
            FrontEnd::BindRejected(e)
                if !uses_deferred_syntax(query) && !is_deferred_bind_error(e) =>
            {
                Verdict::Failed {
                    reason: format!("TCK requires this query to compile, binder rejected it: {e}"),
                }
            }
            FrontEnd::ParseRejected(_) | FrontEnd::BindRejected(_) => {
                Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
            }
            FrontEnd::Accepted => Verdict::Unsupported(UnsupportedReason::Executor),
        },
        Expectation::Error {
            phase: ErrorPhase::AnyTime,
            ..
        } => match &front_end {
            // "Any time" allows compile-time rejection, but the TCK still
            // pins the error class, which we do not verify here.
            FrontEnd::ParseRejected(_) | FrontEnd::BindRejected(_) => {
                Verdict::Unsupported(UnsupportedReason::CompileClassification)
            }
            FrontEnd::Accepted => Verdict::Unsupported(UnsupportedReason::Executor),
        },
        expectation @ (Expectation::Rows { .. } | Expectation::EmptyResult | Expectation::None) => {
            match &front_end {
                FrontEnd::ParseRejected(e) if !uses_deferred_syntax(query) => Verdict::Failed {
                    reason: format!("TCK requires this query to be valid, parser rejected it: {e}"),
                },
                FrontEnd::BindRejected(e)
                    if !uses_deferred_syntax(query) && !is_deferred_bind_error(e) =>
                {
                    Verdict::Failed {
                        reason: format!(
                            "TCK requires this query to be valid, binder rejected it: {e}"
                        ),
                    }
                }
                FrontEnd::ParseRejected(_) | FrontEnd::BindRejected(_) => {
                    Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
                }
                FrontEnd::Accepted => execute_and_verify(plan, query, expectation),
            }
        }
    }
}

/// Execute a front-end-accepted scenario when its fixtures need nothing
/// the executor lacks (graph setup needs the Phase 3 write path; named
/// fixture graphs, parameter tables and stub procedures need harness
/// plumbing), and verify the result against the expected table.
fn execute_and_verify(plan: &ScenarioPlan, query: &str, expectation: &Expectation) -> Verdict {
    let executable = plan.setup_queries.is_empty()
        && plan.named_graph.is_none()
        && !plan.has_parameters
        && !plan.needs_procedures;
    if !executable {
        return Verdict::Unsupported(UnsupportedReason::Executor);
    }
    let outcome = exec::run_query(query, &EmptyGraph, &std::collections::BTreeMap::new());
    let result = match outcome {
        // Feature gaps stay Unsupported; genuine runtime failures on a
        // scenario the TCK requires to succeed are defects.
        Err(exec::QueryError::Exec(ExecError::Unsupported { .. })) => {
            return Verdict::Unsupported(UnsupportedReason::Executor);
        }
        Err(e) => {
            return Verdict::Failed {
                reason: format!("TCK requires this query to succeed, execution failed: {e}"),
            };
        }
        Ok(result) => result,
    };
    match expectation {
        Expectation::EmptyResult => {
            if result.rows.is_empty() {
                Verdict::Passed
            } else {
                Verdict::Failed {
                    reason: format!("expected an empty result, got {} rows", result.rows.len()),
                }
            }
        }
        Expectation::Rows {
            header,
            rows,
            ordered,
            lists_unordered,
        } => {
            if *lists_unordered {
                // List-order-insensitive comparison is not modelled yet.
                return Verdict::Unsupported(UnsupportedReason::Executor);
            }
            match expected::parse_table(header, rows, *ordered) {
                Err(expected::ExpectedError::UnsupportedNotation(_)) => {
                    Verdict::Unsupported(UnsupportedReason::Executor)
                }
                Err(expected::ExpectedError::Malformed(cell)) => Verdict::Failed {
                    reason: format!("harness cannot parse expected cell {cell:?}"),
                },
                Ok(table) => match expected::compare(&table, &result) {
                    None => Verdict::Passed,
                    Some(mismatch) => Verdict::Failed {
                        reason: format!("result mismatch: {mismatch}"),
                    },
                },
            }
        }
        // No modelled outcome: executing without error is all we can
        // check, which is not verification.
        Expectation::None => Verdict::Unsupported(UnsupportedReason::Executor),
        Expectation::Error { .. } => unreachable!("handled by caller"),
    }
}

enum FrontEnd {
    ParseRejected(acetone_cypher::ParseError),
    BindRejected(BindError),
    Accepted,
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

    fn rows(header: &[&str], cells: &[&[&str]], ordered: bool) -> Expectation {
        Expectation::Rows {
            header: header.iter().map(|s| s.to_string()).collect(),
            rows: cells
                .iter()
                .map(|row| row.iter().map(|s| s.to_string()).collect())
                .collect(),
            ordered,
            lists_unordered: false,
        }
    }

    #[test]
    fn valid_query_that_fails_to_parse_is_a_failure() {
        // A read query the TCK expects to succeed; if the parser rejected
        // it, that must surface as Failed, not vanish into Unsupported.
        // (An empty graph makes MATCH yield no rows: expected table empty.)
        let verdict = classify(&plan("MATCH (n) RETURN n", rows(&["n"], &[], false)));
        assert_eq!(verdict, Verdict::Passed);

        let verdict = classify(&plan("MATCH (n) RETURN n AS", rows(&["n"], &[], false)));
        assert!(matches!(verdict, Verdict::Failed { .. }));
    }

    #[test]
    fn deferred_syntax_is_unsupported_not_failed() {
        let verdict = classify(&plan("CREATE (n) RETURN n", rows(&["n"], &[], false)));
        assert_eq!(
            verdict,
            Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
        );
        let verdict = classify(&plan(
            "FOREACH (x IN [1] | SET n.p = x)",
            rows(&["n"], &[], false),
        ));
        assert_eq!(
            verdict,
            Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
        );
    }

    #[test]
    fn execution_verifies_results() {
        // A pure-expression scenario is executed and verified for real.
        let verdict = classify(&plan("RETURN 1 + 2 AS x", rows(&["x"], &[&["3"]], false)));
        assert_eq!(verdict, Verdict::Passed);
        // A wrong expectation is a failure, not an accident.
        let verdict = classify(&plan("RETURN 1 + 2 AS x", rows(&["x"], &[&["4"]], false)));
        assert!(matches!(verdict, Verdict::Failed { .. }));
        // Integer vs float expectations are distinct.
        let verdict = classify(&plan("RETURN 2 ^ 2 AS x", rows(&["x"], &[&["4"]], false)));
        assert!(matches!(verdict, Verdict::Failed { .. }));
        let verdict = classify(&plan("RETURN 2 ^ 2 AS x", rows(&["x"], &[&["4.0"]], false)));
        assert_eq!(verdict, Verdict::Passed);
    }

    #[test]
    fn deferred_keyword_detection_is_token_based() {
        assert!(!uses_deferred_syntax(
            "MATCH (n) RETURN n.offset, n.created"
        ));
        assert!(uses_deferred_syntax("MATCH (n) SET n.p = 1"));
    }
}
