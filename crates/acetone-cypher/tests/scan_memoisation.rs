//! Anchor-scan memoisation (acetone-7qw.6/7qw.7): a fresh (unbound) anchor in
//! expression position — a pattern comprehension or pattern predicate — is
//! evaluated once per outer row, and each evaluation used to re-materialise
//! the whole candidate node set from the source. Over a store-backed source a
//! candidate costs a full node materialisation (30–47 µs measured), so the
//! deterministic scan budget was reached through minutes of real work
//! (bounded, not defended — the Phase 9 security review's HIGH finding).
//!
//! The fix memoises label-scan results for the lifetime of one clause
//! evaluation context, while the governor's deterministic charges stay
//! byte-identical: the budget trips at exactly the same point, but each
//! charged unit now costs a cache lookup, not a store read.

use std::cell::Cell;
use std::collections::BTreeMap;

use acetone_cypher::ast::Direction;
use acetone_cypher::bind::binder::{BindMode, bind};
use acetone_cypher::exec::source::{GraphSource, MemoryGraph};
use acetone_cypher::exec::value::{EntityId, NodeValue, RelValue};
use acetone_cypher::exec::{
    NoProcedures, QueryLimits, SingleVersion, Value, catalogue_from_schema, execute_versioned_with,
    execute_with_limits,
};
use acetone_model::schema::{LabelDef, PropertyType, SchemaEntry};

/// Wraps a graph and counts how many times the whole node set is actually
/// materialised from the source. The executor may route a label scan through
/// wrapper sources whose default `nodes_by_labels` filters `all_nodes`, so
/// the innermost `all_nodes` call is the one true proxy for "a full
/// materialisation reached the store".
struct CountingSource {
    inner: MemoryGraph,
    materialisations: Cell<usize>,
    expands: Cell<usize>,
}

impl CountingSource {
    fn new(inner: MemoryGraph) -> Self {
        CountingSource {
            inner,
            materialisations: Cell::new(0),
            expands: Cell::new(0),
        }
    }
}

impl GraphSource for CountingSource {
    fn all_nodes(&self) -> Vec<NodeValue> {
        self.materialisations.set(self.materialisations.get() + 1);
        self.inner.all_nodes()
    }

    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        self.expands.set(self.expands.get() + 1);
        self.inner.expand(node, direction, types)
    }

    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        self.inner.node(id)
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

/// Three `:N` nodes, one edge n0 → n1.
fn small_graph() -> MemoryGraph {
    let mut g = MemoryGraph::new();
    let n0 = g.add_node(["N"], BTreeMap::from([("id".to_owned(), Value::Int(0))]));
    let n1 = g.add_node(["N"], BTreeMap::from([("id".to_owned(), Value::Int(1))]));
    g.add_node(["N"], BTreeMap::from([("id".to_owned(), Value::Int(2))]));
    g.add_rel(&n0, "R", &n1, BTreeMap::new());
    g
}

/// The single integer a one-cell result holds (`Value` has no `PartialEq` —
/// openCypher equality is three-valued — so tests unwrap by shape).
fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

fn run(query: &str, graph: &dyn GraphSource) -> Result<Vec<Vec<Value>>, String> {
    let ast = acetone_cypher::parse(query).map_err(|e| format!("{e:?}"))?;
    let catalogue = catalogue_from_schema(schema());
    let bound = bind(query, &ast, &catalogue, BindMode::Strict).map_err(|e| format!("{e:?}"))?;
    let resolver = SingleVersion::new(graph);
    execute_versioned_with(&bound, &resolver, &NoProcedures, &BTreeMap::new())
        .map(|r| r.rows)
        .map_err(|e| format!("{e:?}"))
}

#[test]
fn comprehension_fresh_anchor_materialises_once_per_clause_not_per_row() {
    // The governed pathology's shape: a pattern comprehension with a fresh
    // anchor, evaluated once per UNWIND row (here inside an aggregate, as in
    // the measured exploit). 50 rows used to mean 50 full scans.
    let graph = CountingSource::new(small_graph());
    let rows = run(
        "UNWIND range(1, 50) AS i RETURN count(size([(x)-->(y) | 1])) AS c",
        &graph,
    )
    .expect("query executes");
    // One row: count over 50 rows, each comprehension finding the 1 edge.
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows[0][0]), 50);
    assert_eq!(
        graph.materialisations.get(),
        1,
        "a fresh comprehension anchor must materialise the node set once per \
         clause, not once per row"
    );
}

#[test]
fn pattern_predicate_fresh_anchor_materialises_once_per_clause_not_per_row() {
    // The sibling expression-position path (`pattern_exists`): a pattern
    // predicate with an anonymous start node, evaluated once per row.
    let graph = CountingSource::new(small_graph());
    let rows = run(
        "UNWIND range(1, 50) AS i WITH i WHERE (:N)-->() RETURN count(i) AS c",
        &graph,
    )
    .expect("query executes");
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows[0][0]), 50);
    assert_eq!(
        graph.materialisations.get(),
        1,
        "a fresh pattern-predicate anchor must materialise the node set once \
         per clause, not once per row"
    );
}

#[test]
fn distinct_label_sets_are_cached_separately() {
    // Two different anchors in one clause: each label set scans once.
    let graph = CountingSource::new(small_graph());
    let rows = run(
        "UNWIND range(1, 10) AS i \
         RETURN count(size([(x:N)-->(y) | 1])) AS a, count(size([(p)-->(q) | 1])) AS b",
        &graph,
    )
    .expect("query executes");
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows[0][0]), 10);
    assert_eq!(int(&rows[0][1]), 10);
    assert_eq!(
        graph.materialisations.get(),
        2,
        "distinct label sets are distinct cache entries — one scan each"
    );
}

#[test]
fn comprehension_expansion_probes_once_per_anchor_per_clause_not_per_row() {
    // The other half of the shipped-path cost (found by measurement): each
    // anchor candidate costs an `expand()` probe against the source — a
    // store read — and a fresh-anchor comprehension repeats every probe per
    // outer row. Anchors with no edges charge no hops, so this work was both
    // uncharged and unmemoised: 20k probes × N rows was the real 700-second
    // pathology. Per-context memoisation caps it at one probe per distinct
    // (node, direction, types) per clause.
    let graph = CountingSource::new(small_graph());
    let rows = run(
        "UNWIND range(1, 50) AS i RETURN count(size([(x)-->(y) | 1])) AS c",
        &graph,
    )
    .expect("query executes");
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&rows[0][0]), 50);
    assert_eq!(
        graph.expands.get(),
        3,
        "3 anchors probe once each for the whole clause, not once per row"
    );
}

#[test]
fn cache_hits_still_charge_the_deterministic_scan_budget() {
    // The budget semantics must be byte-identical with memoisation: each
    // evaluation still charges the full candidate count, so the same query
    // trips ScannedCandidates at the same point as before — it just gets
    // there in cache lookups instead of store materialisations.
    // 3 nodes × 50 rows = 150 candidates charged; cap at 100 must trip.
    let graph = CountingSource::new(small_graph());
    let ast = acetone_cypher::parse("UNWIND range(1, 50) AS i RETURN count(size([(x)-->(y) | 1]))")
        .expect("parse");
    let catalogue = catalogue_from_schema(schema());
    let bound = bind(
        "UNWIND range(1, 50) AS i RETURN count(size([(x)-->(y) | 1]))",
        &ast,
        &catalogue,
        BindMode::Strict,
    )
    .expect("bind");
    let limits = QueryLimits::unbounded().with_max_scanned_candidates(100);
    let err = execute_with_limits(&bound, &graph, &BTreeMap::new(), &limits)
        .expect_err("the scan budget must still trip on cache hits");
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("ScannedCandidates"),
        "expected ScannedCandidates, got: {rendered}"
    );
}

#[test]
fn writes_are_visible_to_later_clauses_no_stale_cache() {
    // Guard: memoisation must never serve a scan from before a write. A
    // fresh-anchor read after CREATE sees the created node.
    let graph = small_graph();
    let rows = run(
        "CREATE (:N {id: 99}) WITH 1 AS one MATCH (x:N) RETURN count(x) AS c",
        &graph,
    )
    .expect("query executes");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        int(&rows[0][0]),
        4,
        "the clause after CREATE must see 3 base + 1 created :N nodes"
    );
}
