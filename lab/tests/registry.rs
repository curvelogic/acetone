//! The lab graph builds and every registry query runs correctly under
//! Strict binding against the declared schema, at a small deterministic
//! scale.

use std::collections::BTreeMap;

use acetone_cypher::bind::{BindMode, Catalogue, bind};
use acetone_cypher::exec::value::Value;
use acetone_cypher::exec::{GraphSnapshot, catalogue_from_schema, execute};
use acetone_graph::{InitOptions, Repository};

fn build_lab(scale: usize) -> (tempfile::TempDir, GraphSnapshot, Catalogue, (usize, usize)) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(&dir.path().join("repo"), InitOptions::default()).expect("init");
    let counts = acetone_lab::build(&repo, acetone_lab::Shape::from_scale(scale)).expect("build");
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let nodes = snapshot.nodes().expect("nodes");
    let edges = snapshot.edges().expect("edges");
    let graph = GraphSnapshot::from_records(&nodes, &edges);
    let catalogue = catalogue_from_schema(snapshot.schema_entries().expect("schema"));
    (dir, graph, catalogue, counts)
}

fn run(graph: &GraphSnapshot, catalogue: &Catalogue, cypher: &str) -> Vec<Vec<Value>> {
    let parsed = acetone_cypher::parse(cypher).expect("parse");
    let bound = bind(cypher, &parsed, catalogue, BindMode::Strict).expect("bind (strict)");
    execute(&bound, graph, &BTreeMap::new())
        .expect("execute")
        .rows
}

#[test]
fn lab_graph_builds_deterministically_and_queries_bind_strict() {
    let (_dir, graph, catalogue, (reported_nodes, reported_edges)) = build_lab(300);

    // The generator's reported counts match what was actually stored
    // (RUNS deduplication keeps the edge count exact).
    assert_eq!(graph.node_count(), reported_nodes);
    assert_eq!(graph.rel_count(), reported_edges);

    // Every registry query binds Strict and executes.
    for (name, cypher) in acetone_lab::registry_queries() {
        let parsed = acetone_cypher::parse(cypher).unwrap_or_else(|e| panic!("{name}: {e}"));
        let bound = bind(cypher, &parsed, &catalogue, BindMode::Strict)
            .unwrap_or_else(|e| panic!("{name} must bind strict: {e}"));
        execute(&bound, &graph, &BTreeMap::new())
            .unwrap_or_else(|e| panic!("{name} must execute: {e}"));
    }
}

#[test]
fn certificate_expiry_sweep_is_correct() {
    let (_dir, graph, catalogue, _) = build_lab(300);
    let rows = run(
        &graph,
        &catalogue,
        "MATCH (h:Host)-[:HAS_CERT]->(c:Certificate) \
         WHERE c.not_after < 30 AND NOT h.decommissioned \
         RETURN c.not_after AS na, h.decommissioned AS dead ORDER BY na",
    );
    assert!(!rows.is_empty(), "some certs should be expiring");
    for row in &rows {
        // Every returned cert really is under the deadline and its host live.
        assert!(matches!(row[0], Value::Int(n) if n < 30), "not_after < 30");
        assert!(
            matches!(row[1], Value::Bool(false)),
            "host not decommissioned"
        );
    }
}

#[test]
fn indexed_host_count_matches_the_generator() {
    let (_dir, graph, catalogue, _) = build_lab(300);
    // The generator assigns OS round-robin over 5 values, so debian hosts
    // are exactly those with index % 5 == 0.
    let expected = (0..300).filter(|i| i % 5 == 0).count() as i64;
    let rows = run(
        &graph,
        &catalogue,
        "MATCH (h:Host {os: 'debian'}) RETURN count(*) AS n",
    );
    assert_eq!(rows.len(), 1);
    assert!(
        matches!(rows[0][0], Value::Int(n) if n == expected),
        "debian host count"
    );
}

#[test]
fn strict_binding_rejects_an_undeclared_label() {
    let (_dir, graph, catalogue, _) = build_lab(50);
    let _ = &graph;
    let cypher = "MATCH (x:Undeclared) RETURN x";
    let parsed = acetone_cypher::parse(cypher).unwrap();
    // The schema is declared, so Strict binding must reject an unknown
    // label — evidence that the lab graph exercises Strict mode.
    assert!(bind(cypher, &parsed, &catalogue, BindMode::Strict).is_err());
    // The same query binds fine leniently.
    assert!(bind(cypher, &parsed, &catalogue, BindMode::Lenient).is_ok());
}
