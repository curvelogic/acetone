//! The read-path executor (spec §5.3): openCypher value semantics,
//! expression evaluation, pattern matching and the clause pipeline, over
//! a provider-pluggable [`source::GraphSource`].

pub mod eval;
pub mod functions;
pub mod run;
pub mod source;
pub mod value;

pub use eval::{ExecError, Row};
pub use run::{QueryResult, execute};
pub use source::{EmptyGraph, GraphSource, MemoryGraph};
pub use value::Value;

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

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error(transparent)]
    Parse(crate::error::ParseError),
    #[error(transparent)]
    Bind(crate::bind::BindError),
    #[error(transparent)]
    Exec(ExecError),
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
    }
}
