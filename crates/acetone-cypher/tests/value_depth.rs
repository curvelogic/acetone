//! Regression tests for the runtime value construction depth cap
//! (acetone-19x). A shallow AST can build an arbitrarily deep runtime value
//! (`reduce(s = [0], y IN range(1, 200000) | [s])`); before the cap, walking
//! that value in DISTINCT / grouping / ORDER BY (or merely cloning or
//! dropping it) recursed 200k deep and aborted the process with a stack
//! overflow under the shipped default limits. The cap makes such values
//! unrepresentable: construction refuses past `MAX_VALUE_DEPTH` (256) with
//! the executor's typed resource error, so every recursive walk stays
//! bounded.

use std::collections::BTreeMap;

use acetone_cypher::exec::{
    EmptyGraph, ExecError, QueryError, QueryResult, ResourceLimit, Value, run_query,
};

fn params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

fn run(query: &str) -> Result<QueryResult, QueryError> {
    run_query(query, &EmptyGraph, &params())
}

fn expect_depth_error(query: &str) {
    match run(query) {
        Err(QueryError::Exec(ExecError::ResourceExceeded {
            limit: ResourceLimit::ValueDepth,
            ..
        })) => {}
        Err(other) => panic!("expected the value-depth error for {query}, got {other:?}"),
        Ok(_) => panic!("expected the value-depth error for {query}, got success"),
    }
}

/// The container nesting depth of a value, walked iteratively so the test
/// itself can never overflow.
fn nesting_depth(value: &Value) -> usize {
    let mut deepest = 0usize;
    let mut stack = vec![(value, 1usize)];
    while let Some((value, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        match value {
            Value::List(items) => stack.extend(items.iter().map(|v| (v, depth + 1))),
            Value::Map(map) => stack.extend(map.values().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    deepest
}

// --- The three crashing queries from the PR #176 review (pre-existing on
// --- main): each SIGABRTed the process before the construction cap. They
// --- must now fail fast with the typed error — the reduce refuses to nest
// --- past the cap on iteration ~255, long before 200k.

#[test]
fn deep_reduce_through_distinct_errors_instead_of_aborting() {
    expect_depth_error("RETURN DISTINCT reduce(s = [0], y IN range(1, 200000) | [s]) AS g");
}

#[test]
fn deep_reduce_through_grouping_errors_instead_of_aborting() {
    expect_depth_error("RETURN reduce(s = [0], y IN range(1, 200000) | [s]) AS g, count(*) AS c");
}

#[test]
fn deep_reduce_through_order_by_errors_instead_of_aborting() {
    expect_depth_error("RETURN reduce(s = [0], y IN range(1, 200000) | [s]) AS g ORDER BY g");
}

// --- Boundary: the cap is exactly MAX_VALUE_DEPTH (256) container levels.
// --- `reduce(s = [0], …range(1, n)… | [s])` yields depth n + 2, so n = 254
// --- is the deepest constructible value and n = 255 the shallowest refusal.

#[test]
fn nesting_up_to_the_cap_is_constructible() {
    let result = run("RETURN reduce(s = [0], y IN range(1, 254) | [s]) AS g")
        .expect("depth 256 is exactly the cap and must construct");
    assert_eq!(nesting_depth(&result.rows[0][0]), 256);
}

#[test]
fn nesting_past_the_cap_is_refused_at_construction() {
    expect_depth_error("RETURN reduce(s = [0], y IN range(1, 255) | [s]) AS g");
}

// --- Every construction seam that can deepen a value is guarded, not just
// --- the list literal the reduce pathology uses.

#[test]
fn map_literal_growth_is_capped() {
    expect_depth_error("RETURN reduce(m = {a: 1}, y IN range(1, 300) | {a: m}) AS m");
}

#[test]
fn list_push_concat_arm_is_capped() {
    // Build a map at exactly the cap, then push it into a list with `+`:
    // the (List, element) arm must refuse the extra level.
    expect_depth_error(
        "WITH reduce(m = {a: 1}, y IN range(1, 255) | {a: m}) AS m RETURN [] + m AS l",
    );
}

#[test]
fn collect_of_at_cap_values_is_capped() {
    expect_depth_error(
        "WITH reduce(s = [0], y IN range(1, 254) | [s]) AS g RETURN collect(g) AS c",
    );
}

#[test]
fn list_comprehension_element_is_capped() {
    expect_depth_error(
        "WITH reduce(s = [0], y IN range(1, 254) | [s]) AS g RETURN [x IN [1] | g] AS c",
    );
}

// --- Legitimate nesting keeps working, through exactly the walks that used
// --- to crash.

#[test]
fn hundred_deep_nesting_flows_through_distinct_grouping_and_order_by() {
    let query = "WITH reduce(s = [0], y IN range(1, 100) | [s]) AS g \
                 RETURN DISTINCT g AS g, count(*) AS c ORDER BY g";
    let result = run(query).expect("100-deep nesting is legitimate");
    assert_eq!(result.rows.len(), 1);
    assert_eq!(nesting_depth(&result.rows[0][0]), 102);
    assert!(matches!(result.rows[0][1], Value::Int(1)));
}

#[test]
fn collect_below_the_cap_still_works() {
    let result = run("WITH reduce(s = [0], y IN range(1, 253) | [s]) AS g RETURN collect(g) AS c")
        .expect("collect of a depth-255 value yields depth 256, exactly the cap");
    assert_eq!(nesting_depth(&result.rows[0][0]), 256);
}

// --- The cap must not disturb flat, wide accumulation — only depth.

#[test]
fn flat_accumulation_is_unaffected() {
    let result = run("RETURN reduce(s = [], y IN range(1, 1000) | s + y) AS l")
        .expect("a wide flat list is depth 2 regardless of length");
    let Value::List(items) = &result.rows[0][0] else {
        panic!("expected a list");
    };
    assert_eq!(items.len(), 1000);
}

#[test]
fn wide_shallow_literals_are_unaffected() {
    let result = run("RETURN [[1, 2], [3, 4], {a: [5]}] AS v").expect("shallow nesting");
    // list -> map -> list -> scalar: four levels under the scalar-counts-one
    // metric.
    assert_eq!(nesting_depth(&result.rows[0][0]), 4);
}
