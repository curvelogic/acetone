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
fn the_phase_2_review_repro_is_bounded_under_the_shipped_defaults() {
    // The acetone-18z acceptance case, exactly as the Phase 2 security review
    // measured it: `MATCH (a)-[*]->(b)` on the complete 9-node/72-edge digraph
    // did not finish in 20 seconds ungoverned. Under the *shipped default*
    // limits (not an injected small cap) it must return a clean typed
    // ResourceExceeded promptly — the expansion-step cap is the dimension
    // that trips.
    let graph = complete_graph(9);
    let err = run_query_with_limits(
        "MATCH (a:N)-[*]->(b:N) RETURN count(*) AS c",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect_err("the K9 unbounded walk must be governed under the defaults");
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
fn a_list_returning_function_is_charged_against_the_collection_cap() {
    // acetone-fab: the list-returning builtins (split/keys/labels/nodes/
    // relationships/reverse/tail) now charge their result length against the
    // collection cap, like range()/collect(). A `split` into more parts than the
    // cap must be rejected up front, not allocated first.
    let graph = MemoryGraph::new();
    let limits = QueryLimits {
        max_collection_len: 5,
        ..QueryLimits::unbounded()
    };
    // Eight parts, cap of five.
    let err = run_query_with_limits(
        "RETURN split('a,a,a,a,a,a,a,a', ',') AS parts",
        &graph,
        &params(),
        &limits,
    )
    .expect_err("a split past the collection cap must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::CollectionLen);

    // A split within the cap still succeeds (no over-charging).
    run_query_with_limits(
        "RETURN split('a,a,a', ',') AS parts",
        &graph,
        &params(),
        &limits,
    )
    .expect("a split within the cap must succeed");
}

#[test]
fn an_amplifying_replace_is_charged_before_allocation() {
    // replace(s, from, to) can amplify its input (acetone-v3k): each 'a'
    // becomes six bytes here, 4 -> 24, past a cap of 20. The result length is
    // charged against the collection cap *before* the string is built, the
    // same accounting `s + s` and split() pay.
    let graph = MemoryGraph::new();
    let limits = QueryLimits {
        max_collection_len: 20,
        ..QueryLimits::unbounded()
    };
    let err = run_query_with_limits(
        "RETURN replace('aaaa', 'a', 'bbbbbb') AS r",
        &graph,
        &params(),
        &limits,
    )
    .expect_err("an amplifying replace must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::CollectionLen);

    // A replace within the cap still succeeds, unchanged (no over-charging).
    let result = run_query_with_limits(
        "RETURN replace('aa', 'a', 'bbb') AS r",
        &graph,
        &params(),
        &limits,
    )
    .expect("a replace within the cap must succeed");
    assert!(matches!(&result.rows[0][0], Value::String(s) if s == "bbbbbb"));
}

#[test]
fn an_empty_pattern_replace_charges_its_boundary_insertions() {
    // Rust's str::replace with an empty pattern inserts `to` at every char
    // boundary including both ends: replace('ab', '', 'XY') = 'XYaXYbXY',
    // 8 bytes. The pre-charge must use that same length — reject at a cap of
    // 7, accept at 8.
    let graph = MemoryGraph::new();
    let reject = QueryLimits {
        max_collection_len: 7,
        ..QueryLimits::unbounded()
    };
    let err = run_query_with_limits(
        "RETURN replace('ab', '', 'XY') AS r",
        &graph,
        &params(),
        &reject,
    )
    .expect_err("an empty-pattern replace past the cap must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::CollectionLen);

    let accept = QueryLimits {
        max_collection_len: 8,
        ..QueryLimits::unbounded()
    };
    let result = run_query_with_limits(
        "RETURN replace('ab', '', 'XY') AS r",
        &graph,
        &params(),
        &accept,
    )
    .expect("an empty-pattern replace at the cap must succeed");
    assert!(matches!(&result.rows[0][0], Value::String(s) if s == "XYaXYbXY"));
}

#[test]
fn a_doubling_replace_is_bounded_under_the_defaults() {
    // The amplification pathology iterated: each fold step doubles the string
    // via replace, so 40 steps would build 2^40 bytes. The per-call result
    // charge must trip the default collection cap long before memory does —
    // the replace analogue of the doubling `s + s` reduce.
    let graph = MemoryGraph::new();
    let err = run_query_with_limits(
        "RETURN size(reduce(s = 'x', y IN range(1, 40) | replace(s, 'x', 'xx'))) AS n",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect_err("a doubling replace must be governed");
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
fn a_distinct_aggregate_charges_the_collection_cap() {
    // count(DISTINCT x) deduped with an O(n²) linear scan that charged the
    // governor NOTHING (Phase 7 security review HIGH, acetone-8ln). It now
    // charges one unit per distinct value, so an oversized DISTINCT set trips
    // the collection cap instead of grinding uncharged. Under unbounded rows
    // and work, only the DISTINCT charge can trip here: the domain is built
    // from two small ranges (each far under the cap, unlike a single
    // range(0, 5000) whose own up-front charge would trip the cap first and
    // leave the DISTINCT charge untested).
    let graph = MemoryGraph::new();
    let limits = QueryLimits {
        max_collection_len: 1000,
        ..QueryLimits::unbounded()
    };
    let err = run_query_with_limits(
        "UNWIND range(0, 70) AS x UNWIND range(0, 70) AS y \
         RETURN count(DISTINCT x * 100 + y) AS c",
        &graph,
        &params(),
        &limits,
    )
    .expect_err("a DISTINCT aggregate over a huge domain must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::CollectionLen);
}

#[test]
fn a_huge_grouping_set_charges_the_collection_cap() {
    // Aggregation grouping used a BTreeMap over the global sort order, doing
    // O(n log n) global_cmp work the odometer never saw (acetone-bzr). It now
    // hash-groups on the same canonical key DISTINCT uses and charges each
    // new group against the collection cap as it is created. Rows and work
    // are unbounded here and each source range is far under the cap, so only
    // the per-group charge can trip.
    let graph = MemoryGraph::new();
    let limits = QueryLimits {
        max_collection_len: 1000,
        ..QueryLimits::unbounded()
    };
    let err = run_query_with_limits(
        "UNWIND range(0, 99) AS x UNWIND range(0, 99) AS y \
         RETURN x * 100 + y AS g, count(*) AS c",
        &graph,
        &params(),
        &limits,
    )
    .expect_err("a huge grouping set must be governed");
    assert_eq!(resource_limit(err), ResourceLimit::CollectionLen);
}

#[test]
fn grouped_aggregation_folds_groups_correctly() {
    // Correctness through the hash-keyed grouping rewrite (acetone-bzr):
    // duplicates fold into one group, an integer and its float image group
    // together (Int(1) ≡ Float(1.0), as for DISTINCT), and a realistic
    // grouped aggregation passes untouched under the shipped defaults.
    let graph = MemoryGraph::new();
    let result = run_query_with_limits(
        "UNWIND [1, 2, 1.0, 3, 2, 1] AS x RETURN x AS v, count(*) AS c ORDER BY v",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect("grouped aggregation must pass under defaults");
    assert_eq!(result.rows.len(), 3);
    assert!(matches!(result.rows[0][1], Value::Int(3))); // 1, 1.0, 1
    assert!(matches!(result.rows[1][1], Value::Int(2))); // 2, 2
    assert!(matches!(result.rows[2][1], Value::Int(1))); // 3
}

#[test]
fn a_huge_distinct_projection_runs_in_linear_time() {
    // The reviewer's repro: `UNWIND range(0, N) RETURN DISTINCT x`. The old
    // linear-scan dedup did ~N² comparisons — for N=100k that is 10^10 ops,
    // minutes of CPU while the odometer read ~N. The hash-keyed dedup is O(n),
    // so this returns all 100_001 distinct rows near-instantly; on the old code
    // the test would not finish inside the suite's time budget (acetone-8ln).
    let graph = MemoryGraph::new();
    let result = run_query_with_limits(
        "UNWIND range(0, 100000) AS x RETURN DISTINCT x",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect("a large DISTINCT must complete under the defaults");
    assert_eq!(result.rows.len(), 100_001);
}

#[test]
fn distinct_dedups_correctly_across_the_number_domain() {
    // Correctness is preserved through the hash-key rewrite: duplicates fold,
    // and an integer and its float image are one value (Int(1) ≡ Float(1.0)).
    let graph = MemoryGraph::new();
    let result = run_query_with_limits(
        "UNWIND [1, 1, 1.0, 2, 3, 3, 2] AS x RETURN DISTINCT x AS v ORDER BY v",
        &graph,
        &params(),
        &QueryLimits::default(),
    )
    .expect("distinct dedup");
    // {1, 2, 3} — 1 and 1.0 collapse to a single value.
    assert_eq!(result.rows.len(), 3);
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
