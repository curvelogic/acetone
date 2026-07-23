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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use acetone_cypher::bind::{BindError, BindMode, Catalogue, bind};
use acetone_cypher::exec::value::{EntityId, Value};
use acetone_cypher::exec::{
    self, EmptyGraph, ExecError, GraphSource, MemoryGraph, SingleVersion, execute_write,
};

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
    /// Uses syntax acetone defers: spec §5.1 deferrals, or write syntax
    /// beyond the v0.1 Level W subset (the supported write clauses now
    /// route to write-verify instead — acetone-1h7).
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
        // The Level W write clauses (CREATE/MERGE/SET/REMOVE/DELETE/DETACH)
        // now parse, bind, execute and verify (acetone-1h7), so they are no
        // longer deferred — write scenarios route to the write-verify path.
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
                // A write query the front end rejects uses write syntax
                // beyond the v0.1 Level W subset (undirected MERGE, `SET
                // (n).p`, `WITH *` after a write, …) — deferred, like a read
                // deferral, not a defect.
                FrontEnd::ParseRejected(e)
                    if !uses_deferred_syntax(query) && !is_write_query(query) =>
                {
                    Verdict::Failed {
                        reason: format!(
                            "TCK requires this query to be valid, parser rejected it: {e}"
                        ),
                    }
                }
                FrontEnd::BindRejected(e)
                    if !uses_deferred_syntax(query)
                        && !is_write_query(query)
                        && !is_deferred_bind_error(e) =>
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
                FrontEnd::Accepted if uses_deferred_syntax(query) => {
                    Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
                }
                // Write scenarios (acetone-1h7) run their setup graph and
                // the query under test in memory, then verify both the
                // returned rows and the openCypher side effects.
                FrontEnd::Accepted if is_write_query(query) => {
                    write_verify(plan, query, expectation)
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
        && plan.controls.is_empty()
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
    verify_rows(expectation, &result)
}

/// Token heuristic: does the query contain a write clause? (`DETACH` only
/// ever accompanies `DELETE`, so `DELETE` covers it.)
fn is_write_query(query: &str) -> bool {
    const WRITE: &[&str] = &["CREATE", "MERGE", "SET", "REMOVE", "DELETE"];
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|token| WRITE.iter().any(|kw| token.eq_ignore_ascii_case(kw)))
}

/// A write query's execution failure, distinguishing a front-end rejection
/// (only reachable for setup statements) from an executor error.
enum WriteRunError {
    Front,
    Exec(ExecError),
}

/// Parse, bind (leniently, schema-free like the rest of the TCK backend)
/// and execute one write statement against `graph`, returning its result
/// and net changes.
fn run_write_once(
    query: &str,
    graph: &MemoryGraph,
    params: &std::collections::BTreeMap<String, acetone_cypher::exec::Value>,
) -> Result<(exec::QueryResult, exec::WriteChanges), WriteRunError> {
    let parsed = acetone_cypher::parse(query).map_err(|_| WriteRunError::Front)?;
    let bound = bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient)
        .map_err(|_| WriteRunError::Front)?;
    let resolver = SingleVersion::new(graph);
    execute_write(&bound, &resolver, params).map_err(WriteRunError::Exec)
}

/// Run a write scenario end to end in memory (acetone-1h7): build the setup
/// graph one statement at a time, execute the query under test, and verify
/// both its rows and its openCypher side effects. Fixtures needing a named
/// graph, parameters or stub procedures remain beyond the harness.
///
/// Side effects are read from the *graph delta* (state before the query vs
/// state after its changes are applied), not from the executor's per-op
/// counters, because openCypher counts graph-state changes: distinct label
/// tokens (`CREATE (:L),(:L)` is `+labels 1`), net node identities
/// (`CREATE (n) DELETE n` is `+nodes 0`) and net property values.
fn write_verify(plan: &ScenarioPlan, query: &str, expectation: &Expectation) -> Verdict {
    if plan.named_graph.is_some() || plan.has_parameters || plan.needs_procedures {
        return Verdict::Unsupported(UnsupportedReason::Executor);
    }
    let params = BTreeMap::new();
    let mut graph = MemoryGraph::new();
    // A setup statement the front end or executor cannot handle means the
    // fixture cannot be built — Unsupported, not a defect in the query.
    for setup in &plan.setup_queries {
        match run_write_once(setup, &graph, &params) {
            Ok((_, changes)) => graph.apply(&changes),
            Err(_) => return Verdict::Unsupported(UnsupportedReason::Executor),
        }
    }
    let before = GraphMetrics::of(&graph);
    let (result, changes) = match run_write_once(query, &graph, &params) {
        Ok(pair) => pair,
        Err(WriteRunError::Exec(ExecError::Unsupported { .. })) | Err(WriteRunError::Front) => {
            return Verdict::Unsupported(UnsupportedReason::Executor);
        }
        Err(WriteRunError::Exec(e)) => {
            return Verdict::Failed {
                reason: format!("TCK requires this write to succeed, execution failed: {e}"),
            };
        }
    };
    // A write scenario asserts its side effects even when it returns no
    // rows, so check them first (against the graph delta).
    graph.apply(&changes);
    if let Some(expected) = &plan.side_effects {
        let actual = GraphMetrics::of(&graph).delta_from(&before);
        if let Some(reason) = compare_side_effects(expected, &actual) {
            return Verdict::Failed { reason };
        }
    }
    let main = verify_rows(expectation, &result);
    if main != Verdict::Passed {
        return main;
    }
    // Control queries verify the graph the write left behind: each runs
    // read-only against the post-write state, with its own expectation.
    for check in &plan.controls {
        if matches!(
            check.expectation,
            Expectation::Error { .. } | Expectation::None
        ) {
            // No control table the harness models — not verifiable.
            return Verdict::Unsupported(UnsupportedReason::Executor);
        }
        let (control_result, _) = match run_write_once(&check.query, &graph, &params) {
            Ok(pair) => pair,
            Err(WriteRunError::Exec(ExecError::Unsupported { .. })) | Err(WriteRunError::Front) => {
                return Verdict::Unsupported(UnsupportedReason::Executor);
            }
            Err(WriteRunError::Exec(e)) => {
                return Verdict::Failed {
                    reason: format!("control query failed to execute: {e}"),
                };
            }
        };
        match verify_rows(&check.expectation, &control_result) {
            Verdict::Passed => {}
            Verdict::Failed { reason } => {
                return Verdict::Failed {
                    reason: format!("control query: {reason}"),
                };
            }
            unsupported => return unsupported,
        }
    }
    Verdict::Passed
}

/// A snapshot of the graph properties openCypher side effects are counted
/// against: node and relationship identities, the set of distinct label
/// tokens, and every element property keyed by `(element id, key)`.
struct GraphMetrics {
    node_ids: HashSet<EntityId>,
    rel_ids: HashSet<EntityId>,
    labels: HashSet<String>,
    props: HashMap<(EntityId, String), Value>,
}

impl GraphMetrics {
    fn of(graph: &MemoryGraph) -> Self {
        let mut m = GraphMetrics {
            node_ids: HashSet::new(),
            rel_ids: HashSet::new(),
            labels: HashSet::new(),
            props: HashMap::new(),
        };
        // A null-valued property does not exist in openCypher (`{p: null}`
        // stores nothing), so it must not count towards `±properties`.
        for node in graph.all_nodes() {
            m.node_ids.insert(node.id.clone());
            for label in &node.labels {
                m.labels.insert(label.clone());
            }
            for (key, value) in &node.properties {
                if !value.is_null() {
                    m.props
                        .insert((node.id.clone(), key.clone()), value.clone());
                }
            }
        }
        for rel in graph.all_rels() {
            m.rel_ids.insert(rel.id.clone());
            for (key, value) in &rel.properties {
                if !value.is_null() {
                    m.props.insert((rel.id.clone(), key.clone()), value.clone());
                }
            }
        }
        m
    }

    /// The openCypher side effects that turn `before` into `self`, named as
    /// the TCK does (`+nodes`, `-properties`, …); zero effects are omitted.
    fn delta_from(&self, before: &GraphMetrics) -> BTreeMap<String, u64> {
        let mut effects = BTreeMap::new();
        let mut put = |name: &str, count: usize| {
            if count > 0 {
                effects.insert(name.to_string(), count as u64);
            }
        };
        put("+nodes", self.node_ids.difference(&before.node_ids).count());
        put("-nodes", before.node_ids.difference(&self.node_ids).count());
        put(
            "+relationships",
            self.rel_ids.difference(&before.rel_ids).count(),
        );
        put(
            "-relationships",
            before.rel_ids.difference(&self.rel_ids).count(),
        );
        put("+labels", self.labels.difference(&before.labels).count());
        put("-labels", before.labels.difference(&self.labels).count());
        // openCypher counts properties per operation, not by net state:
        // overwriting an existing value both removes the old (`-1`) and sets
        // the new (`+1`). So a *changed* value counts on both sides; a newly
        // present one only on `+`, and a vanished one only on `-`.
        let changed = self
            .props
            .iter()
            .filter(|(k, v)| {
                before
                    .props
                    .get(*k)
                    .is_some_and(|old| !values_equal(old, v))
            })
            .count();
        let newly_present = self
            .props
            .iter()
            .filter(|(k, _)| !before.props.contains_key(*k))
            .count();
        let gone = before
            .props
            .keys()
            .filter(|k| !self.props.contains_key(*k))
            .count();
        put("+properties", newly_present + changed);
        put("-properties", gone + changed);
        effects
    }
}

/// Structural equality of two property values. `Value` has no `PartialEq`
/// (three-valued comparison; `Float`), so compare canonical debug forms —
/// exact for the property value types (primitives and lists thereof), and
/// type-distinct (`1` ≠ `1.0`).
fn values_equal(a: &Value, b: &Value) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

/// Compare expected against actual side effects: every named effect must
/// match, and any effect not named must be zero. Returns a mismatch
/// description, or `None` on an exact match.
fn compare_side_effects(
    expected: &BTreeMap<String, u64>,
    actual: &BTreeMap<String, u64>,
) -> Option<String> {
    let names: BTreeSet<&String> = expected.keys().chain(actual.keys()).collect();
    for name in names {
        let want = expected.get(name).copied().unwrap_or(0);
        let got = actual.get(name).copied().unwrap_or(0);
        if want != got {
            return Some(format!("side effect {name}: expected {want}, got {got}"));
        }
    }
    None
}

/// Verify a query's result rows against the scenario's expected table.
fn verify_rows(expectation: &Expectation, result: &exec::QueryResult) -> Verdict {
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
                Ok(table) => match expected::compare(&table, result) {
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
            controls: Vec::new(),
            side_effects: None,
        }
    }

    fn compile_error(error_type: &str) -> Expectation {
        Expectation::Error {
            error_type: error_type.into(),
            phase: ErrorPhase::CompileTime,
            detail: "whatever".into(),
        }
    }

    /// A write scenario: optional setup graph, the query under test, its
    /// expected rows and side effects.
    fn write_plan(
        setup: &[&str],
        query: &str,
        expectation: Expectation,
        effects: &[(&str, u64)],
    ) -> ScenarioPlan {
        let mut p = plan(query, expectation);
        p.setup_queries = setup.iter().map(|s| s.to_string()).collect();
        p.side_effects = Some(effects.iter().map(|(k, v)| (k.to_string(), *v)).collect());
        p
    }

    fn empty_rows(cols: &[&str]) -> Expectation {
        Expectation::Rows {
            header: cols.iter().map(|c| c.to_string()).collect(),
            rows: vec![],
            ordered: false,
            lists_unordered: false,
        }
    }

    #[test]
    fn create_side_effects_are_verified() {
        // +nodes, +labels (one distinct token per label name), +properties.
        let v = classify(&write_plan(
            &[],
            "CREATE (:A:B {x: 1, y: 2})",
            Expectation::EmptyResult,
            &[("+nodes", 1), ("+labels", 2), ("+properties", 2)],
        ));
        assert_eq!(v, Verdict::Passed);
    }

    #[test]
    fn duplicate_label_token_counts_once() {
        // openCypher counts distinct label tokens, not per-node labels.
        let v = classify(&write_plan(
            &[],
            "CREATE (:L), (:L)",
            Expectation::EmptyResult,
            &[("+nodes", 2), ("+labels", 1)],
        ));
        assert_eq!(v, Verdict::Passed);
    }

    #[test]
    fn created_then_deleted_node_nets_out() {
        let v = classify(&write_plan(
            &[],
            "CREATE (n) DELETE n",
            Expectation::EmptyResult,
            &[],
        ));
        assert_eq!(v, Verdict::Passed);
    }

    #[test]
    fn null_valued_property_is_not_counted() {
        let v = classify(&write_plan(
            &[],
            "CREATE ({id: 12, name: null})",
            Expectation::EmptyResult,
            &[("+nodes", 1), ("+properties", 1)],
        ));
        assert_eq!(v, Verdict::Passed);
    }

    #[test]
    fn setup_graph_and_overwrite_counts_both_sides() {
        // Overwriting an existing value is `+properties 1` and
        // `-properties 1`; the setup graph is built first.
        let v = classify(&write_plan(
            &["CREATE (:N {num: 42})"],
            "MATCH (n:N) SET n.num = 43 RETURN n LIMIT 0",
            empty_rows(&["n"]),
            &[("+properties", 1), ("-properties", 1)],
        ));
        assert_eq!(v, Verdict::Passed);
    }

    #[test]
    fn delete_of_a_node_with_a_property_removes_it() {
        let v = classify(&write_plan(
            &["CREATE (:N {num: 42})"],
            "MATCH (n:N) DELETE n",
            Expectation::EmptyResult,
            &[("-nodes", 1), ("-labels", 1), ("-properties", 1)],
        ));
        assert_eq!(v, Verdict::Passed);
    }

    #[test]
    fn a_wrong_side_effect_count_fails() {
        let v = classify(&write_plan(
            &[],
            "CREATE (:A)",
            Expectation::EmptyResult,
            &[("+nodes", 2)],
        ));
        assert!(matches!(v, Verdict::Failed { .. }));
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
        // FOREACH remains deferred syntax.
        let verdict = classify(&plan(
            "FOREACH (x IN [1] | SET n.p = x)",
            rows(&["n"], &[], false),
        ));
        assert_eq!(
            verdict,
            Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
        );
        // A write query whose syntax is beyond the v0.1 subset (undirected
        // MERGE) is Unsupported, not Failed.
        let verdict = classify(&plan(
            "MATCH (a), (b) MERGE (a)-[r:R]-(b) RETURN r",
            rows(&["r"], &[], false),
        ));
        assert_eq!(
            verdict,
            Verdict::Unsupported(UnsupportedReason::DeferredSyntax)
        );
        // But a write clause we support is verified, not bucketed
        // (acetone-1h7).
        let verdict = classify(&write_plan(
            &[],
            "CREATE (:A)",
            Expectation::EmptyResult,
            &[("+nodes", 1), ("+labels", 1)],
        ));
        assert_eq!(verdict, Verdict::Passed);
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
        // A still-deferred construct is detected by token.
        assert!(uses_deferred_syntax("FOREACH (x IN [1] | SET n.p = x)"));
        // The now-supported write clauses are no longer deferred (they route
        // to write-verify instead).
        assert!(!uses_deferred_syntax("MATCH (n) SET n.p = 1"));
    }
}
