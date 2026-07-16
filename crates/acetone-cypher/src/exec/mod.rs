//! The read-path executor (spec §5.3): openCypher value semantics,
//! expression evaluation, pattern matching and the clause pipeline, over
//! a provider-pluggable [`source::GraphSource`].

pub mod adapter;
pub mod eval;
pub mod functions;
pub mod governor;
pub mod run;
pub mod source;
pub mod store_source;
pub mod value;
pub mod write;

pub use adapter::{GraphSnapshot, catalogue_from_schema, virtual_diff_node};
pub use eval::{ExecError, ResourceLimit, Row};
pub use governor::{Governor, QueryLimits};
pub use run::{
    QueryResult, execute, execute_versioned, execute_versioned_with, execute_versioned_with_limits,
    execute_with_governor, execute_with_limits, execute_write, execute_write_with_limits,
};
pub use source::{
    EmptyGraph, GraphSource, MemoryGraph, NoProcedures, ProcedureProvider, SingleVersion,
    VersionResolver,
};
pub use store_source::StoreBackedSource;
pub use value::Value;
pub use write::{MutableGraph, Mutation, WriteChanges, WriteSummary};

/// Parse, bind (lenient) and execute a query against a graph — the
/// convenience path used by tests and the TCK backend.
pub fn run_query(
    query_text: &str,
    graph: &dyn GraphSource,
    parameters: &std::collections::BTreeMap<String, Value>,
) -> Result<QueryResult, QueryError> {
    let parsed = crate::parse(query_text).map_err(QueryError::Parse)?;
    let bound = crate::bind::bind(
        query_text,
        &parsed,
        &crate::bind::Catalogue::empty(),
        crate::bind::BindMode::Lenient,
    )
    .map_err(QueryError::Bind)?;
    execute(&bound, graph, parameters).map_err(QueryError::Exec)
}

/// Like [`run_query`] but under explicit [`QueryLimits`] — the convenience
/// path the governor tests drive to prove the caps end to end.
pub fn run_query_with_limits(
    query_text: &str,
    graph: &dyn GraphSource,
    parameters: &std::collections::BTreeMap<String, Value>,
    limits: &QueryLimits,
) -> Result<QueryResult, QueryError> {
    let parsed = crate::parse(query_text).map_err(QueryError::Parse)?;
    let bound = crate::bind::bind(
        query_text,
        &parsed,
        &crate::bind::Catalogue::empty(),
        crate::bind::BindMode::Lenient,
    )
    .map_err(QueryError::Bind)?;
    run::execute_with_limits(&bound, graph, parameters, limits).map_err(QueryError::Exec)
}

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error(transparent)]
    Parse(crate::error::ParseError),
    #[error(transparent)]
    Bind(crate::bind::BindError),
    #[error(transparent)]
    Exec(ExecError),
}

impl QueryError {
    /// Render with 1-based line/column against the source, delegating to the
    /// wrapped layer's `render` so a read query's parse, bind and execution
    /// errors are all located the same way.
    pub fn render(&self, source: &str) -> String {
        match self {
            QueryError::Parse(e) => e.render(source),
            QueryError::Bind(e) => e.render(source),
            QueryError::Exec(e) => e.render(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn run(query: &str) -> QueryResult {
        run_query(query, &EmptyGraph, &BTreeMap::new()).expect(query)
    }

    fn single(query: &str) -> Value {
        let result = run(query);
        assert_eq!(result.rows.len(), 1, "{query}");
        result.rows[0][0].clone()
    }

    fn host_graph() -> MemoryGraph {
        let mut graph = MemoryGraph::new();
        let mut props = BTreeMap::new();
        props.insert("name".to_string(), Value::String("a".into()));
        let a = graph.add_node(["Host"], props);
        let mut props = BTreeMap::new();
        props.insert("name".to_string(), Value::String("b".into()));
        let b = graph.add_node(["Host", "Web"], props);
        let c = graph.add_node(["Software"], BTreeMap::new());
        graph.add_rel(&a, "RUNS", &c, BTreeMap::new());
        graph.add_rel(&b, "RUNS", &c, BTreeMap::new());
        graph
    }

    #[test]
    fn pure_expressions_execute() {
        assert!(matches!(single("RETURN 1 + 2 * 3"), Value::Int(7)));
        assert!(matches!(single("RETURN 'a' + 'b'"), Value::String(s) if s == "ab"));
        assert!(single("RETURN null + 1").is_null());
        assert!(matches!(single("RETURN 2 ^ 3"), Value::Float(x) if x == 8.0));
        assert!(matches!(single("RETURN 1 = 1.0"), Value::Bool(true)));
        assert!(single("RETURN null = null").is_null());
        assert!(matches!(single("RETURN 1 < 'a'"), Value::Null));
        assert!(matches!(single("RETURN NOT null"), Value::Null));
        assert!(matches!(single("RETURN 3 IN [1, 2, 3]"), Value::Bool(true)));
        assert!(single("RETURN 4 IN [1, null]").is_null());
        assert!(matches!(
            single("RETURN CASE WHEN false THEN 1 ELSE 2 END"),
            Value::Int(2)
        ));
        assert!(matches!(
            single("RETURN [x IN range(1, 5) WHERE x % 2 = 0 | x * 10]"),
            Value::List(items) if items.len() == 2
        ));
        assert!(matches!(single("RETURN size('héllo')"), Value::Int(5)));
        assert!(matches!(single("RETURN [1,2,3][-1]"), Value::Int(3)));
        assert!(matches!(single("RETURN [1,2,3][5]"), Value::Null));
    }

    #[test]
    fn quantifiers_with_three_valued_logic() {
        // any: at least one true.
        assert!(matches!(
            single("RETURN any(x IN [1,2,3] WHERE x > 2)"),
            Value::Bool(true)
        ));
        assert!(matches!(
            single("RETURN any(x IN [1,2] WHERE x > 5)"),
            Value::Bool(false)
        ));
        assert!(matches!(
            single("RETURN any(x IN [] WHERE x > 0)"),
            Value::Bool(false)
        ));
        // null propagation: no true but a null predicate -> null.
        assert!(single("RETURN any(x IN [1, null] WHERE x > 5)").is_null());
        // all: every element true.
        assert!(matches!(
            single("RETURN all(x IN [2,3,4] WHERE x > 1)"),
            Value::Bool(true)
        ));
        assert!(matches!(
            single("RETURN all(x IN [1,2] WHERE x > 1)"),
            Value::Bool(false)
        ));
        assert!(matches!(
            single("RETURN all(x IN [] WHERE x > 1)"),
            Value::Bool(true)
        ));
        assert!(single("RETURN all(x IN [2, null] WHERE x > 1)").is_null());
        // none = not any.
        assert!(matches!(
            single("RETURN none(x IN [1,2] WHERE x > 5)"),
            Value::Bool(true)
        ));
        assert!(matches!(
            single("RETURN none(x IN [1,6] WHERE x > 5)"),
            Value::Bool(false)
        ));
        // single: exactly one true.
        assert!(matches!(
            single("RETURN single(x IN [1,2,3] WHERE x = 2)"),
            Value::Bool(true)
        ));
        assert!(matches!(
            single("RETURN single(x IN [2,2] WHERE x = 2)"),
            Value::Bool(false)
        ));
        assert!(matches!(
            single("RETURN single(x IN [1] WHERE x = 9)"),
            Value::Bool(false)
        ));
        // null list -> null.
        assert!(single("RETURN any(x IN null WHERE x > 0)").is_null());
    }

    #[test]
    fn reduce_folds_a_list() {
        assert!(matches!(
            single("RETURN reduce(acc = 0, x IN [1,2,3,4] | acc + x)"),
            Value::Int(10)
        ));
        assert!(matches!(
            single("RETURN reduce(s = '', x IN ['a','b','c'] | s + x)"),
            Value::String(s) if s == "abc"
        ));
        // Empty list yields the initial accumulator.
        assert!(matches!(
            single("RETURN reduce(acc = 7, x IN [] | acc + x)"),
            Value::Int(7)
        ));
    }

    #[test]
    fn unwind_and_aggregates() {
        let result = run("UNWIND [1, 2, 2, null] AS x RETURN count(x), sum(x), collect(x)");
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0][0], Value::Int(3)));
        assert!(matches!(result.rows[0][1], Value::Int(5)));
        assert!(matches!(&result.rows[0][2], Value::List(items) if items.len() == 3));

        let result = run("UNWIND [1, 2, 2] AS x RETURN count(DISTINCT x) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(2)));

        // count over an empty input is 0, one row.
        let result = run("UNWIND [] AS x RETURN count(x) AS c");
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0][0], Value::Int(0)));
    }

    #[test]
    fn grouping_by_non_aggregated_items() {
        let result = run("UNWIND [['a', 1], ['b', 2], ['a', 3]] AS pair \
             RETURN pair[0] AS k, sum(pair[1]) AS total ORDER BY k");
        assert_eq!(result.rows.len(), 2);
        assert!(matches!(&result.rows[0][0], Value::String(s) if s == "a"));
        assert!(matches!(result.rows[0][1], Value::Int(4)));
        assert!(matches!(result.rows[1][1], Value::Int(2)));
    }

    #[test]
    fn match_expand_and_filter() {
        let graph = host_graph();
        let result = run_query(
            "MATCH (h:Host)-[:RUNS]->(s:Software) RETURN h.name ORDER BY h.name",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(result.rows.len(), 2);
        assert!(matches!(&result.rows[0][0], Value::String(s) if s == "a"));

        let result = run_query(
            "MATCH (h:Host) WHERE h.name = 'b' RETURN labels(h)",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], Value::List(labels) if labels.len() == 2));
    }

    #[test]
    fn var_length_over_created_edges_traverses_each_once() {
        // Created edges carry stable overlay ids (acetone-rid); a var-length
        // walk over them must honour relationship uniqueness across the
        // overlay — each created edge traversed at most once, no double count.
        let result = run("CREATE (a:N)-[:R]->(b:N) CREATE (b)-[:R]->(c:N) \
             WITH a MATCH p = (a)-[:R*]->(x) RETURN count(p) AS c");
        // Paths from a: a->b (len 1) and a->b->c (len 2) = 2, each edge once.
        assert!(matches!(result.rows[0][0], Value::Int(2)));
    }

    #[test]
    fn optional_match_extends_with_nulls() {
        let graph = host_graph();
        let result = run_query(
            "MATCH (s:Software) OPTIONAL MATCH (s)-[:MISSING]->(x) RETURN s, x",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0][0], Value::Node(_)));
        assert!(result.rows[0][1].is_null());
    }

    #[test]
    fn var_length_and_paths() {
        let mut graph = MemoryGraph::new();
        let a = graph.add_node(["N"], BTreeMap::new());
        let b = graph.add_node(["N"], BTreeMap::new());
        let c = graph.add_node(["N"], BTreeMap::new());
        graph.add_rel(&a, "R", &b, BTreeMap::new());
        graph.add_rel(&b, "R", &c, BTreeMap::new());

        let result = run_query(
            "MATCH p = (a:N)-[:R*1..2]->(b:N) RETURN length(p) ORDER BY length(p)",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        // Paths: a->b, b->c (length 1 each), a->b->c (length 2).
        assert_eq!(result.rows.len(), 3);
        assert!(matches!(result.rows[2][0], Value::Int(2)));
    }

    #[test]
    fn distinct_order_skip_limit() {
        let result =
            run("UNWIND [3, 1, 2, 3, 1] AS x RETURN DISTINCT x ORDER BY x DESC SKIP 1 LIMIT 2");
        assert_eq!(result.rows.len(), 2);
        assert!(matches!(result.rows[0][0], Value::Int(2)));
        assert!(matches!(result.rows[1][0], Value::Int(1)));
    }

    #[test]
    fn null_ordering_places_null_last_ascending() {
        let result = run("UNWIND [null, 1, 2] AS x RETURN x ORDER BY x");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        assert!(result.rows[2][0].is_null());
    }

    #[test]
    fn with_pipeline_and_where() {
        let result = run("UNWIND [1, 2, 3, 4] AS x WITH x WHERE x % 2 = 0 RETURN sum(x) AS total");
        assert!(matches!(result.rows[0][0], Value::Int(6)));
    }

    #[test]
    fn pattern_predicate_probes() {
        let graph = host_graph();
        let result = run_query(
            "MATCH (h:Host) WHERE (h)-[:RUNS]->(:Software) RETURN count(h) AS n",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(matches!(result.rows[0][0], Value::Int(2)));
    }

    #[test]
    fn runtime_errors_surface() {
        let err = run_query("RETURN 1 / 0", &EmptyGraph, &BTreeMap::new()).unwrap_err();
        assert!(matches!(
            err,
            QueryError::Exec(ExecError::DivisionByZero { .. })
        ));
        let err = run_query("RETURN 1 + true", &EmptyGraph, &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, QueryError::Exec(ExecError::Type { .. })));
        // i64::MIN / -1 overflows i64 — an error, not a silent wrap.
        // (i64::MIN has no positive literal, so build it by subtraction.)
        let err = run_query(
            "RETURN (-9223372036854775807 - 1) / -1",
            &EmptyGraph,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::Exec(ExecError::Overflow { .. })));
    }

    // --- CREATE (write path, acetone-mex.1) ---------------------------------

    #[test]
    fn create_then_match_sees_the_new_node() {
        // Writes are visible to later clauses in the same query.
        let result = run("CREATE (a:N {v: 1}) MATCH (n:N) RETURN n.v AS v");
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        assert_eq!(result.stats.nodes_created, 1);
        assert_eq!(result.stats.relationships_created, 0);
    }

    #[test]
    fn create_overlays_the_base_graph() {
        // Two hosts in the base; CREATE adds a third, all visible.
        let graph = host_graph();
        let result = run_query(
            "CREATE (:Host {name: 'c'}) MATCH (h:Host) RETURN count(h) AS n",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(matches!(result.rows[0][0], Value::Int(3)));
        assert_eq!(result.stats.nodes_created, 1);
    }

    #[test]
    fn create_relationship_is_traversable() {
        let result = run("CREATE (a:A)-[:R]->(b:B) \
             MATCH (x:A)-[:R]->(y:B) RETURN count(*) AS n");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        assert_eq!(result.stats.nodes_created, 2);
        assert_eq!(result.stats.relationships_created, 1);
    }

    #[test]
    fn create_reuses_a_bound_node_variable() {
        // The second CREATE references `a` rather than making a new node.
        let result = run("CREATE (a:A) CREATE (a)-[:R]->(b:B) \
             MATCH (x:A)-[:R]->(y:B) RETURN count(*) AS n");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        // Two nodes total (a, b), one relationship — a is not recreated.
        assert_eq!(result.stats.nodes_created, 2);
        assert_eq!(result.stats.relationships_created, 1);
    }

    #[test]
    fn create_binds_a_path_variable() {
        let result = run("CREATE p = (a:A)-[:R]->(b:B) RETURN length(p) AS len");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
    }

    #[test]
    fn create_over_unwind_makes_one_node_per_row() {
        let result = run("UNWIND [1, 2, 3] AS x CREATE (:N {v: x})");
        // No RETURN: an empty result, but three nodes created.
        assert!(result.rows.is_empty());
        assert_eq!(result.stats.nodes_created, 3);
    }

    #[test]
    fn create_incoming_direction_orients_the_edge() {
        // `(a)<-[:R]-(b)` creates b-[:R]->a.
        let result = run("CREATE (a:A)<-[:R]-(b:B) \
             MATCH (x:B)-[:R]->(y:A) RETURN count(*) AS n");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
    }

    // --- SET / REMOVE (write path, acetone-eah) -----------------------------

    #[test]
    fn set_property_then_read_it() {
        let result = run("CREATE (a:N {v: 1}) SET a.v = 42 MATCH (n:N) RETURN n.v AS v");
        assert!(matches!(result.rows[0][0], Value::Int(42)));
        assert!(result.stats.properties_set >= 1);
    }

    #[test]
    fn set_property_on_a_base_node() {
        let graph = host_graph();
        let result = run_query(
            "MATCH (h:Host {name: 'a'}) SET h.os = 'debian' \
             MATCH (x:Host {name: 'a'}) RETURN x.os AS os",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(matches!(&result.rows[0][0], Value::String(s) if s == "debian"));
    }

    #[test]
    fn set_null_removes_a_property() {
        let result = run("CREATE (a:N {v: 1, w: 2}) SET a.v = null RETURN a.v AS v, a.w AS w");
        assert!(result.rows[0][0].is_null());
        assert!(matches!(result.rows[0][1], Value::Int(2)));
    }

    #[test]
    fn set_replace_and_merge_maps() {
        // Replace drops unlisted properties.
        let result = run("CREATE (a:N {x: 1, y: 2}) SET a = {z: 9} RETURN a.x AS x, a.z AS z");
        assert!(result.rows[0][0].is_null());
        assert!(matches!(result.rows[0][1], Value::Int(9)));
        // Merge keeps them.
        let result = run("CREATE (a:N {x: 1, y: 2}) SET a += {y: 5, z: 9} \
             RETURN a.x AS x, a.y AS y, a.z AS z");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        assert!(matches!(result.rows[0][1], Value::Int(5)));
        assert!(matches!(result.rows[0][2], Value::Int(9)));
    }

    #[test]
    fn set_and_remove_labels() {
        let result = run("CREATE (a:N) SET a:Extra:More RETURN labels(a) AS ls");
        let Value::List(labels) = &result.rows[0][0] else {
            panic!("expected a list");
        };
        assert_eq!(labels.len(), 3);
        assert!(result.stats.labels_added >= 2);

        let result = run("CREATE (a:N:Extra) REMOVE a:Extra RETURN labels(a) AS ls");
        let Value::List(labels) = &result.rows[0][0] else {
            panic!("expected a list");
        };
        assert_eq!(labels.len(), 1);
        assert_eq!(result.stats.labels_removed, 1);
    }

    #[test]
    fn remove_property() {
        let result = run("CREATE (a:N {v: 1}) REMOVE a.v RETURN a.v AS v");
        assert!(result.rows[0][0].is_null());
    }

    #[test]
    fn set_items_see_earlier_effects_in_the_same_clause() {
        let result = run("CREATE (a:N {v: 1}) SET a.v = 10, a.w = a.v RETURN a.v AS v, a.w AS w");
        assert!(matches!(result.rows[0][0], Value::Int(10)));
        assert!(matches!(result.rows[0][1], Value::Int(10)));
    }

    #[test]
    fn set_a_relationship_property() {
        let result = run("CREATE (a:A)-[r:R]->(b:B) SET r.w = 7 \
             MATCH (:A)-[e:R]->(:B) RETURN e.w AS w");
        assert!(matches!(result.rows[0][0], Value::Int(7)));
    }

    #[test]
    fn set_on_optional_miss_is_a_noop() {
        // n is null (no Missing node); SET must not error.
        let result = run("OPTIONAL MATCH (n:Nope) SET n.x = 1 RETURN n");
        assert_eq!(result.rows.len(), 1);
        assert!(result.rows[0][0].is_null());
        assert_eq!(result.stats.properties_set, 0);
    }

    #[test]
    fn merge_null_is_a_noop() {
        let result = run("CREATE (a:N {v: 1}) SET a += null RETURN a.v AS v");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
    }

    #[test]
    fn at_snapshot_survives_a_later_write() {
        // A node bound from an AT <ref> version must keep its historical
        // values after an unrelated SET, even though it shares identity with
        // a base node (Invariant #3). Regression for the refresh_entities
        // override-only fix.
        use crate::bind::{BindMode, Catalogue, bind};
        use crate::exec::source::VersionResolver;

        // Base: a Host (n0, v="new") and a Marker (n1). Old: a Host (n0,
        // v="old"). The AT Host and base Host share id "n0"; the Marker
        // (n1), which the query mutates, is distinct.
        fn base_graph() -> MemoryGraph {
            let mut g = MemoryGraph::new();
            let mut p = BTreeMap::new();
            p.insert("v".to_string(), Value::String("new".into()));
            g.add_node(["Host"], p); // n0
            g.add_node(["Marker"], BTreeMap::new()); // n1
            g
        }
        fn old_graph() -> MemoryGraph {
            let mut g = MemoryGraph::new();
            let mut p = BTreeMap::new();
            p.insert("v".to_string(), Value::String("old".into()));
            g.add_node(["Host"], p); // n0
            g
        }
        struct R {
            base: MemoryGraph,
        }
        impl VersionResolver for R {
            fn base(&self) -> &dyn GraphSource {
                &self.base
            }
            fn at(&self, refspec: &str) -> Result<Box<dyn GraphSource>, String> {
                match refspec {
                    "old" => Ok(Box::new(old_graph())),
                    other => Err(format!("no such version '{other}'")),
                }
            }
        }
        let resolver = R { base: base_graph() };
        let q = "MATCH (h:Host) AT 'old' MATCH (c:Marker) SET c.x = 1 RETURN h.v AS v";
        let parsed = crate::parse(q).unwrap();
        let bound = bind(q, &parsed, &Catalogue::empty(), BindMode::Lenient).unwrap();
        let result = execute_versioned(&bound, &resolver, &BTreeMap::new()).unwrap();
        // Must be the AT-version value, not the base's "new".
        assert!(
            matches!(&result.rows[0][0], Value::String(s) if s == "old"),
            "AT snapshot was clobbered by the later write: {:?}",
            result.rows[0][0]
        );
    }

    // --- DELETE / DETACH DELETE (write path, acetone-921) -------------------

    #[test]
    fn delete_a_node_then_it_is_gone() {
        let result = run("CREATE (a:N) CREATE (b:N) DELETE a MATCH (n:N) RETURN count(n) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        assert_eq!(result.stats.nodes_deleted, 1);
    }

    #[test]
    fn delete_a_base_node() {
        let graph = host_graph();
        // Every Host has an outgoing RUNS edge, so DETACH is required.
        let result = run_query(
            "MATCH (h:Host {name: 'a'}) DETACH DELETE h \
             MATCH (n:Host) RETURN count(n) AS c",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(matches!(result.rows[0][0], Value::Int(1)));
    }

    #[test]
    fn delete_connected_node_without_detach_errors() {
        let err = crate::exec::run_query(
            "CREATE (a:A)-[:R]->(b:B) DELETE a",
            &EmptyGraph,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            crate::exec::QueryError::Exec(crate::exec::ExecError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn detach_delete_removes_incident_edges() {
        let result = run("CREATE (a:A)-[:R]->(b:B) DETACH DELETE a \
             MATCH (:A)-[r:R]->(:B) RETURN count(r) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(0)));
        assert_eq!(result.stats.nodes_deleted, 1);
        assert_eq!(result.stats.relationships_deleted, 1);
    }

    #[test]
    fn delete_relationship_then_endpoints_are_free() {
        // Deleting the relationship first lets a plain DELETE of the node
        // succeed in the same clause.
        let result = run("CREATE (a:A)-[r:R]->(b:B) DELETE r, a, b \
             MATCH (n) RETURN count(n) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(0)));
        assert_eq!(result.stats.relationships_deleted, 1);
        assert_eq!(result.stats.nodes_deleted, 2);
    }

    #[test]
    fn plain_delete_of_a_node_and_all_its_relationships() {
        // `a` has two outgoing edges, matched over two rows. Deleting the
        // relationship and `a` in one clause must succeed without DETACH —
        // the connectivity check is deferred to clause end (regression for
        // the per-row eager-check bug).
        let result = run("CREATE (a:A)-[:R]->(b:B) CREATE (a)-[:R]->(c:B) \
             WITH a MATCH (a)-[r:R]->(x) DELETE r, a \
             MATCH (n:A) RETURN count(n) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(0)));
        assert_eq!(result.stats.nodes_deleted, 1);
        assert_eq!(result.stats.relationships_deleted, 2);
    }

    #[test]
    fn delete_on_null_is_a_noop() {
        let result = run("OPTIONAL MATCH (n:Nope) DELETE n RETURN 1 AS x");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        assert_eq!(result.stats.nodes_deleted, 0);
    }

    #[test]
    fn delete_a_path_deletes_all_elements() {
        let result = run("CREATE p = (a:A)-[:R]->(b:B) DETACH DELETE p \
             MATCH (n) RETURN count(n) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(0)));
        assert_eq!(result.stats.nodes_deleted, 2);
        assert_eq!(result.stats.relationships_deleted, 1);
    }

    // --- MERGE (write path, acetone-k0i) ------------------------------------

    #[test]
    fn merge_creates_when_absent_matches_when_present() {
        // First MERGE creates; the second matches it (idempotent within a
        // query) — one node total.
        let result = run("MERGE (a:N {k: 1}) MERGE (b:N {k: 1}) \
             MATCH (n:N) RETURN count(n) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        assert_eq!(result.stats.nodes_created, 1);
    }

    #[test]
    fn merge_matches_an_existing_base_node() {
        let graph = host_graph();
        // A Host named 'a' already exists: MERGE matches, creates nothing.
        let result = run_query(
            "MERGE (h:Host {name: 'a'}) RETURN h.name AS n",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(matches!(&result.rows[0][0], Value::String(s) if s == "a"));
        assert_eq!(result.stats.nodes_created, 0);
    }

    #[test]
    fn merge_on_create_and_on_match_set() {
        // Created: ON CREATE fires.
        let result = run(
            "MERGE (a:N {k: 1}) ON CREATE SET a.tag = 'new' ON MATCH SET a.tag = 'old' \
             RETURN a.tag AS t",
        );
        assert!(matches!(&result.rows[0][0], Value::String(s) if s == "new"));

        // Matched: ON MATCH fires.
        let result = run("CREATE (:N {k: 1}) \
             MERGE (a:N {k: 1}) ON CREATE SET a.tag = 'new' ON MATCH SET a.tag = 'old' \
             RETURN a.tag AS t");
        assert!(matches!(&result.rows[0][0], Value::String(s) if s == "old"));
    }

    #[test]
    fn merge_a_relationship_reusing_a_bound_node() {
        // a exists; MERGE (a)-[:R]->(b) creates the missing rel and b.
        let result = run("CREATE (a:A {k: 1}) \
             WITH a MERGE (a)-[:R]->(b:B) \
             MATCH (:A)-[r:R]->(:B) RETURN count(r) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        // Re-MERGE the same relationship: no new one.
        let result = run("CREATE (a:A {k: 1}) \
             WITH a MERGE (a)-[:R]->(b:B) \
             WITH a MERGE (a)-[:R]->(c:B) \
             MATCH (:A)-[r:R]->(:B) RETURN count(r) AS c");
        assert!(matches!(result.rows[0][0], Value::Int(1)));
    }

    #[test]
    fn merge_is_idempotent_on_reexecution() {
        // Running the same MERGE against a graph that already has the node
        // creates nothing — the persistence-level idempotence (mex.5) rests
        // on this in-memory guarantee.
        let mut graph = MemoryGraph::new();
        let mut p = BTreeMap::new();
        p.insert("k".to_string(), Value::Int(1));
        graph.add_node(["N"], p);
        let result = run_query(
            "MERGE (n:N {k: 1}) RETURN n.k AS k",
            &graph,
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(matches!(result.rows[0][0], Value::Int(1)));
        assert!(
            result.stats.is_empty(),
            "MERGE of an existing node wrote something"
        );
    }

    #[test]
    fn clause_group_at_queries_another_version() {
        use crate::bind::{BindMode, Catalogue, bind};
        use crate::exec::source::VersionResolver;

        // Two versions: "old" has one Host, "new" (the base) has two.
        fn host_graph(n: usize) -> MemoryGraph {
            let mut graph = MemoryGraph::new();
            for i in 0..n {
                let mut props = BTreeMap::new();
                props.insert("id".to_string(), Value::Int(i as i64));
                graph.add_node(["Host"], props);
            }
            graph
        }

        // NOTE: MemoryGraph identity is a per-graph counter, so
        // cross-version *re-anchoring* would not align here — this test
        // uses distinct variables (n, m) and never re-anchors. Stored
        // graphs (the real path) identify by natural key, which is
        // version-stable, so re-anchoring is sound there (see
        // execute_versioned's doc comment).
        struct TwoVersions {
            base: MemoryGraph,
        }
        impl VersionResolver for TwoVersions {
            fn base(&self) -> &dyn GraphSource {
                &self.base
            }
            fn at(&self, refspec: &str) -> Result<Box<dyn GraphSource>, String> {
                match refspec {
                    "old" => Ok(Box::new(host_graph(1))),
                    other => Err(format!("no such version '{other}'")),
                }
            }
        }
        let resolver = TwoVersions {
            base: host_graph(2),
        };

        let exec = |q: &str| {
            let parsed = crate::parse(q).unwrap();
            let bound = bind(q, &parsed, &Catalogue::empty(), BindMode::Lenient).unwrap();
            execute_versioned(&bound, &resolver, &BTreeMap::new()).unwrap()
        };

        // Base version: two hosts.
        let now = exec("MATCH (h:Host) RETURN count(*) AS n");
        assert!(matches!(now.rows[0][0], Value::Int(2)));

        // Same query AT the old version: one host.
        let past = exec("MATCH (h:Host) AT 'old' RETURN count(*) AS n");
        assert!(matches!(past.rows[0][0], Value::Int(1)));

        // One query spanning both versions.
        let both = exec("MATCH (n:Host) AT 'old' MATCH (m:Host) RETURN count(*) AS pairs");
        // 1 old host × 2 base hosts = 2 rows collapsed to a count.
        assert!(matches!(both.rows[0][0], Value::Int(2)));

        // An unresolvable ref is a clean error.
        let parsed = crate::parse("MATCH (h) AT 'nope' RETURN h").unwrap();
        let bound = bind(
            "MATCH (h) AT 'nope' RETURN h",
            &parsed,
            &Catalogue::empty(),
            BindMode::Lenient,
        )
        .unwrap();
        let err = execute_versioned(&bound, &resolver, &BTreeMap::new()).unwrap_err();
        assert!(matches!(err, ExecError::InvalidArgument { .. }));

        // The single-graph path reports AT as unsupported, not a panic.
        let err =
            run_query("MATCH (h) AT 'x' RETURN h", &EmptyGraph, &BTreeMap::new()).unwrap_err();
        assert!(matches!(
            err,
            QueryError::Exec(ExecError::InvalidArgument { .. })
        ));
    }

    // --- CALL procedures (read path, acetone-8c3) ---------------------------

    /// A stand-in provider returning fixed rows, for executor-level CALL tests
    /// without a repository.
    struct FixedProcedures(Vec<Vec<Value>>);

    impl crate::exec::ProcedureProvider for FixedProcedures {
        fn call(&self, _name: &str, _args: &[Value]) -> Result<Vec<Vec<Value>>, String> {
            Ok(self.0.clone())
        }
    }

    fn call_with(query: &str, provider: &dyn crate::exec::ProcedureProvider) -> QueryResult {
        use crate::bind::{BindMode, Catalogue, bind};
        let parsed = crate::parse(query).expect("parse");
        let bound = bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient).expect("bind");
        crate::exec::execute_versioned_with(
            &bound,
            &SingleVersion::new(&EmptyGraph),
            provider,
            &BTreeMap::new(),
        )
        .expect(query)
    }

    #[test]
    fn call_binds_yields_and_applies_where() {
        // acetone.diff yields (kind, label, key, node); the provider returns
        // two rows, one added and one removed (node column unused here).
        let provider = FixedProcedures(vec![
            vec![
                Value::String("added".into()),
                Value::String("N".into()),
                Value::String("k1".into()),
                Value::Null,
            ],
            vec![
                Value::String("removed".into()),
                Value::String("N".into()),
                Value::String("k2".into()),
                Value::Null,
            ],
        ]);
        // YIELD a subset in a different order, filter with WHERE, then RETURN.
        let result = call_with(
            "CALL acetone.diff('a', 'b') YIELD key, kind WHERE kind = 'added' RETURN key, kind",
            &provider,
        );
        assert_eq!(result.columns, vec!["key", "kind"]);
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], Value::String(s) if s == "k1"));
        assert!(matches!(&result.rows[0][1], Value::String(s) if s == "added"));
    }

    #[test]
    fn call_yields_virtual_diff_nodes() {
        // The diff virtual graph (acetone-14c.1): acetone.diff's `node` column
        // carries the changed node as a value labelled with its change kind,
        // queryable with `'_Added' IN labels(node)`.
        use crate::exec::value::{EntityId, NodeValue};
        let node = |id: i64, kind: &str| {
            Value::Node(NodeValue {
                id: EntityId::from_bytes(format!("n{id}").into_bytes()),
                labels: vec![kind.to_string(), "N".to_string()],
                properties: BTreeMap::from([("id".to_string(), Value::Int(id))]),
            })
        };
        let provider = FixedProcedures(vec![
            vec![
                Value::String("modified".into()),
                Value::String("N".into()),
                Value::String("k1".into()),
                node(1, "_Modified"),
            ],
            vec![
                Value::String("added".into()),
                Value::String("N".into()),
                Value::String("k2".into()),
                node(2, "_Added"),
            ],
        ]);
        let result = call_with(
            "CALL acetone.diff('a', 'b') YIELD node \
             WHERE '_Added' IN labels(node) RETURN node.id AS id",
            &provider,
        );
        assert_eq!(result.columns, vec!["id"]);
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], Value::Int(2)));
    }

    #[test]
    fn standalone_call_projects_declared_yield_columns() {
        // No YIELD, no RETURN: the procedure's declared columns are the result.
        let provider = FixedProcedures(vec![vec![
            Value::String("abc123".into()),
            Value::String("a subject".into()),
        ]]);
        let result = call_with("CALL acetone.log('main')", &provider);
        assert_eq!(result.columns, vec!["commit", "subject"]);
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], Value::String(s) if s == "abc123"));
    }

    #[test]
    fn yield_without_return_projects_the_yielded_columns() {
        let provider = FixedProcedures(vec![vec![
            Value::String("added".into()),
            Value::String("N".into()),
            Value::String("k1".into()),
            Value::Null,
        ]]);
        let result = call_with("CALL acetone.diff('a', 'b') YIELD kind, key", &provider);
        assert_eq!(result.columns, vec!["kind", "key"]);
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn call_without_a_provider_is_a_clean_error() {
        // The default (NoProcedures) path used by tests/TCK errors rather
        // than panicking.
        let err = run_query("CALL acetone.log()", &EmptyGraph, &BTreeMap::new()).unwrap_err();
        assert!(matches!(
            err,
            QueryError::Exec(ExecError::InvalidArgument { .. })
        ));
    }
}
