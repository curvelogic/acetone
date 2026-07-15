//! Crash-hardening regression tests (pre-0.1 review U12/U13): untrusted query
//! input must not crash the process. A deep variable-length walk must not
//! overflow the call stack, and `range()` must not panic on integer overflow or
//! exhaust memory on a huge span.

use std::collections::{BTreeMap, HashMap};

use acetone_cypher::ast::Direction;
use acetone_cypher::bind::binder::{BindMode, bind};
use acetone_cypher::exec::source::GraphSource;
use acetone_cypher::exec::value::{EntityId, NodeValue, RelValue};
use acetone_cypher::exec::{
    NoProcedures, SingleVersion, Value, catalogue_from_schema, execute_versioned_with,
};
use acetone_model::schema::{LabelDef, PropertyType, SchemaEntry};

/// A linear chain of `len` nodes 0 → 1 → … → len-1, each labelled `N` with an
/// integer `id`. Outgoing expansion from node i yields node i+1.
struct ChainSource {
    nodes: Vec<NodeValue>,
    by_id: HashMap<EntityId, usize>,
}

impl ChainSource {
    fn new(len: usize) -> Self {
        let nodes: Vec<NodeValue> = (0..len)
            .map(|i| NodeValue {
                id: EntityId::from_bytes(format!("n{i}").into_bytes()),
                labels: vec!["N".into()],
                properties: BTreeMap::from([("id".to_string(), Value::Int(i as i64))]),
            })
            .collect();
        let by_id = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.clone(), i))
            .collect();
        ChainSource { nodes, by_id }
    }
}

impl GraphSource for ChainSource {
    fn all_nodes(&self) -> Vec<NodeValue> {
        self.nodes.clone()
    }
    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        self.by_id.get(id).map(|&i| self.nodes[i].clone())
    }
    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        _types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        let Some(&i) = self.by_id.get(node) else {
            return Vec::new();
        };
        // Outgoing (or undirected) only, to node i+1.
        if matches!(direction, Direction::In) || i + 1 >= self.nodes.len() {
            return Vec::new();
        }
        let rel = RelValue {
            id: EntityId::from_bytes(format!("e{i}").into_bytes()),
            rel_type: "NEXT".into(),
            start: self.nodes[i].id.clone(),
            end: self.nodes[i + 1].id.clone(),
            properties: BTreeMap::new(),
        };
        vec![(rel, self.nodes[i + 1].clone())]
    }
}

fn schema() -> Vec<SchemaEntry> {
    vec![SchemaEntry::Label {
        name: "N".into(),
        def: LabelDef::new(
            vec!["id".into()],
            BTreeMap::from([("id".to_owned(), PropertyType::Int)]),
            [],
            [],
        )
        .expect("label"),
    }]
}

/// Execute `query` against `graph`, returning the single scalar in the single
/// result row (or an error).
fn run_scalar(query: &str, graph: &dyn GraphSource) -> Result<Value, String> {
    let ast = acetone_cypher::parse(query).map_err(|e| format!("{e:?}"))?;
    let catalogue = catalogue_from_schema(schema());
    let bound = bind(query, &ast, &catalogue, BindMode::Strict).map_err(|e| format!("{e:?}"))?;
    let resolver = SingleVersion::new(graph);
    let result = execute_versioned_with(&bound, &resolver, &NoProcedures, &BTreeMap::new())
        .map_err(|e| format!("{e:?}"))?;
    Ok(result
        .rows
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .unwrap_or(Value::Null))
}

#[test]
fn a_deep_variable_length_walk_does_not_overflow_the_stack() {
    // U12: a `*` walk over a long chain reaches a path length of ~LEN. The former
    // per-hop recursion overflowed the call stack (SIGABRT) at this depth; the
    // explicit-stack DFS keeps only a bounded amount on the OS stack.
    //
    // Run in a thread with a deliberately small stack: a per-hop recursion would
    // overflow it well before depth LEN (aborting the process), while the
    // iterative walk completes comfortably. This keeps LEN — and thus the
    // O(LEN^2) match-state cloning inherent to the current executor — modest and
    // the test fast, without depending on the host's default stack size.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024) // 256 KiB
        .spawn(|| {
            const LEN: usize = 2000;
            let graph = ChainSource::new(LEN);
            let got = run_scalar("MATCH (a:N {id: 0})-[*]->(b) RETURN count(b) AS n", &graph)
                .expect("deep walk executes");
            match got {
                Value::Int(n) => assert_eq!(n, (LEN - 1) as i64),
                other => panic!("expected an integer count, got {other:?}"),
            }
        })
        .expect("spawn worker thread");
    handle
        .join()
        .expect("the walk must complete without overflowing the small stack");
}

#[test]
fn range_over_a_huge_span_is_rejected_not_oom() {
    // U13: range(0, i64::MAX) would materialise ~9.2e18 elements. The resource
    // governor (acetone-iq6) rejects it up front on the collection-size cap.
    let graph = ChainSource::new(1);
    let err = run_scalar("RETURN range(0, 9223372036854775807) AS r", &graph)
        .expect_err("huge range must be rejected");
    assert!(
        err.contains("ResourceExceeded") && err.contains("CollectionLen"),
        "unexpected error: {err}"
    );
}

#[test]
fn range_up_to_i64_max_does_not_overflow() {
    // U13: the former `at += step` panicked on the increment past i64::MAX.
    let graph = ChainSource::new(1);
    let got = run_scalar(
        "RETURN range(9223372036854775806, 9223372036854775807) AS r",
        &graph,
    )
    .expect("range up to i64::MAX executes");
    match got {
        Value::List(items) => assert_eq!(items.len(), 2, "expected two elements"),
        other => panic!("expected a list, got {other:?}"),
    }
}
