//! Integration tests for the query resource governor (acetone-iq6, ADR-0036).
//!
//! These drive the *public* governed entry points end to end and prove the
//! three pathologies from the Phase 2 security review return a bounded
//! `ResourceExceeded` error instead of hanging or OOMing — and that realistic
//! queries stay well under the defaults. Caps here are set small so the tests
//! are fast and can never hang even if a seam is left unguarded.

use std::collections::BTreeMap;
use std::time::Duration;

use acetone_cypher::bind::{BindMode, Catalogue, bind};
use acetone_cypher::exec::{
    ExecError, Governor, MemoryGraph, NoProcedures, QueryError, QueryLimits, ResourceLimit,
    SingleVersion, Value, execute_with_governor, run_query_with_limits,
};
use acetone_cypher::parse;

fn params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

/// A complete directed graph on `n` nodes: every ordered pair has an edge. An
/// unbounded var-length walk over this enumerates a super-exponential number
/// of simple paths — the acetone-18z / MAJOR-1 pathology.
fn complete_graph(n: usize) -> MemoryGraph {
    let mut g = MemoryGraph::new();
    let ids: Vec<_> = (0..n).map(|_| g.add_node(["N"], BTreeMap::new())).collect();
    for (i, from) in ids.iter().enumerate() {
        for (j, to) in ids.iter().enumerate() {
            if i != j {
                g.add_rel(from, "R", to, BTreeMap::new());
            }
        }
    }
    g
}

fn resource_limit(err: QueryError) -> ResourceLimit {
    match err {
        QueryError::Exec(ExecError::ResourceExceeded { limit, .. }) => limit,
        other => panic!("expected a ResourceExceeded error, got {other:?}"),
    }
}

#[test]
fn unbounded_var_length_on_a_dense_graph_is_bounded() {
    // Without a governor this query does not terminate on a dense graph.
    let graph = complete_graph(8);
    let limits = QueryLimits {
        max_expansion_steps: 2_000,
        ..QueryLimits::unbounded()
    };
    let err = run_query_with_limits(
        "MATCH (a:N)-[*]->(b:N) RETURN count(*) AS c",
        &graph,
        &params(),
        &limits,
    )
    .expect_err("an unbounded walk over a dense graph must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::ExpansionSteps);
}

#[test]
fn range_over_a_huge_span_is_bounded() {
    // range(0, 1e10) was OOM-killed at ~80GB before the governor — MAJOR-2.
    // The default collection cap (10M) rejects it up front, before allocation.
    let graph = MemoryGraph::new();
    let err = run_query_with_limits(
        "RETURN range(0, 10000000000) AS r",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect_err("an enormous range() must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::CollectionLen);
}

#[test]
fn a_huge_intermediate_row_set_is_bounded() {
    // A cartesian blow-up whose intermediate set dwarfs the tiny final result:
    // 51 * 51 = 2601 rows materialised, capped at 100.
    let graph = MemoryGraph::new();
    let limits = QueryLimits {
        max_result_rows: 100,
        ..QueryLimits::unbounded()
    };
    let err = run_query_with_limits(
        "UNWIND range(0, 50) AS x UNWIND range(0, 50) AS y RETURN count(*) AS c",
        &graph,
        &params(),
        &limits,
    )
    .expect_err("a huge intermediate row set must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::ResultRows);
}

#[test]
fn the_work_odometer_catches_a_query_that_dodges_the_dimensional_caps() {
    // A moderate expansion that individually stays under the row and hop caps
    // but whose total charged work exceeds a tight odometer still errors —
    // the odometer is the catch-all backstop.
    let graph = complete_graph(6);
    let limits = QueryLimits {
        max_work_units: 500,
        ..QueryLimits::unbounded()
    };
    let err = run_query_with_limits(
        "MATCH (a:N)-[*]->(b:N) RETURN count(*) AS c",
        &graph,
        &params(),
        &limits,
    )
    .expect_err("the work odometer must bound total work");
    assert_eq!(resource_limit(err), ResourceLimit::WorkUnits);
}

#[test]
fn a_doubling_reduce_over_lists_is_bounded() {
    // reduce(acc=[1], x IN range(1,N) | acc + acc) doubles the list each step:
    // N steps build 2^N elements. Without charging `+` this dodged every cap
    // (adversarial review blocker). It must now trip the collection cap.
    let graph = MemoryGraph::new();
    let err = run_query_with_limits(
        "RETURN reduce(acc = [1], x IN range(1, 40) | acc + acc) AS r",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect_err("a doubling reduce over lists must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::CollectionLen);
}

#[test]
fn a_doubling_reduce_over_strings_is_bounded() {
    // The string analogue: s + s doubles the string each step.
    let graph = MemoryGraph::new();
    let err = run_query_with_limits(
        "RETURN size(reduce(s = 'x', y IN range(1, 40) | s + s)) AS n",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect_err("a doubling reduce over strings must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::CollectionLen);
}

#[test]
fn a_mid_size_list_comprehension_passes_under_defaults() {
    // A comprehension of ~20k elements is a legitimate query well under the
    // 10M collection cap. The first fix charged quadratically and killed it at
    // ~14k (adversarial review major); linear charging must let it through,
    // consistently with range() and collect() of the same size.
    let graph = MemoryGraph::new();
    let result = run_query_with_limits(
        "RETURN size([x IN range(0, 20000) | x * 2]) AS n",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect("a mid-size comprehension must pass under defaults");
    assert!(matches!(result.rows[0][0], Value::Int(20001)));
}

#[test]
fn a_registry_scale_query_stays_under_the_defaults() {
    // A realistic lab-graph shape: a few hundred nodes, a bounded traversal.
    // Must succeed under the shipped defaults with room to spare.
    let mut g = MemoryGraph::new();
    let hosts: Vec<_> = (0..200)
        .map(|i| {
            let mut p = BTreeMap::new();
            p.insert("id".to_string(), Value::Int(i));
            g.add_node(["Host"], p)
        })
        .collect();
    let sw = g.add_node(["Software"], BTreeMap::new());
    for h in &hosts {
        g.add_rel(h, "RUNS", &sw, BTreeMap::new());
    }
    let result = run_query_with_limits(
        "MATCH (h:Host)-[:RUNS]->(s:Software) RETURN count(h) AS c",
        &g,
        &params(),
        &QueryLimits::default(),
    )
    .expect("a registry-scale query must pass under defaults");
    assert!(matches!(result.rows[0][0], Value::Int(200)));
}

#[test]
fn the_wall_clock_backstop_trips_on_an_already_past_deadline() {
    // The optional backstop: an effectively-zero budget means the deadline is
    // already past by the time the first poll stride is reached, so a query
    // doing appreciable work errors on the wall clock. Other caps are
    // unbounded, so only the clock can trip.
    let graph = complete_graph(8);
    let limits = QueryLimits {
        wall_clock: Some(Duration::from_nanos(1)),
        ..QueryLimits::unbounded()
    };
    let err = run_query_with_limits(
        "MATCH (a:N)-[*]->(b:N) RETURN count(*) AS c",
        &graph,
        &params(),
        &limits,
    )
    .expect_err("an already-past wall-clock deadline must bound the query");
    assert_eq!(resource_limit(err), ResourceLimit::WallClock);
}

#[test]
fn the_default_path_never_reads_the_wall_clock() {
    // wall_clock defaults to None, so a normal query is deterministic and
    // clock-free — it must simply succeed under the defaults.
    let graph = complete_graph(4);
    let result = run_query_with_limits(
        "MATCH (a:N)-[*1..2]->(b:N) RETURN count(*) AS c",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect("a small bounded walk must pass under defaults");
    assert!(matches!(result.rows[0][0], Value::Int(_)));
}

#[test]
fn charged_work_is_reproducible_across_runs() {
    // The determinism obligation (ADR-0036, Invariant discipline): the same
    // query over the same graph charges identical work and yields identical
    // results on every run — no iteration-order nondeterminism in accounting.
    let graph = complete_graph(5);
    let query = "MATCH (a:N)-[*1..3]->(b:N) RETURN count(*) AS c";
    let parsed = parse(query).unwrap();
    let bound = bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient).unwrap();

    let run = || {
        let governor = Governor::new(QueryLimits::unbounded());
        let (result, _) = execute_with_governor(
            &bound,
            &SingleVersion::new(&graph),
            &NoProcedures,
            &params(),
            &governor,
        )
        .unwrap();
        (format!("{:?}", result.rows[0][0]), governor.work_units())
    };

    let (rows_a, work_a) = run();
    let (rows_b, work_b) = run();
    assert_eq!(work_a, work_b, "charged work must be reproducible");
    assert_eq!(rows_a, rows_b, "results must be reproducible");
    assert!(work_a > 0, "a matching query must charge some work");
}
