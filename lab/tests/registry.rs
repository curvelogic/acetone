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
    let schema = snapshot.schema_entries().expect("schema");
    let graph = GraphSnapshot::from_records_with_schema(&nodes, &edges, &schema);
    let catalogue = catalogue_from_schema(schema);
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
fn orphaned_software_finds_the_seeded_orphans() {
    let (_dir, graph, catalogue, _) = build_lab(300);
    let rows = run(
        &graph,
        &catalogue,
        "MATCH (s:Software) WHERE NOT (s)<-[:RUNS]-(:Host) RETURN count(*) AS n",
    );
    // The generator reserves an orphan tail of software (never RUNS-
    // targeted), so this query has a real non-empty answer.
    assert!(
        matches!(rows[0][0], Value::Int(n) if n > 0),
        "expected some orphans, got {:?}",
        rows[0][0]
    );
}

#[test]
fn supply_chain_blast_radius_traverses_variable_length_deps() {
    let (_dir, graph, catalogue, _) = build_lab(300);
    // Anchoring on a KEY property (Supplier.name) works only because the
    // adapter re-exposes key values as queryable properties.
    let rows = run(
        &graph,
        &catalogue,
        "MATCH (v:Supplier {name: 'supplier-0'})<-[:SUPPLIED_BY]-(s:Software) \
         OPTIONAL MATCH (s)<-[:DEPENDS_ON*0..3]-(top:Software)<-[:RUNS]-(h:Host) \
         RETURN count(DISTINCT h) AS exposed_hosts",
    );
    assert_eq!(rows.len(), 1);
    // supplier-0 supplies software real hosts run (transitively), so the
    // blast radius is non-empty — the var-length traversal did work.
    assert!(
        matches!(rows[0][0], Value::Int(n) if n > 0),
        "blast radius should be non-empty, got {:?}",
        rows[0][0]
    );
}

#[test]
fn key_properties_are_filterable_and_returnable() {
    let (_dir, graph, catalogue, _) = build_lab(300);
    // Host's key is `hostname`; the generator stores host keys as
    // "host-<i>". Filtering and returning the key must work.
    let rows = run(
        &graph,
        &catalogue,
        "MATCH (h:Host {hostname: 'host-7'}) RETURN h.hostname AS hn, h.os AS os",
    );
    assert_eq!(rows.len(), 1, "exactly one host has that key");
    assert!(
        matches!(&rows[0][0], Value::String(s) if s == "host-7"),
        "key returnable"
    );
    assert!(
        matches!(&rows[0][1], Value::String(_)),
        "non-key property still returnable"
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

/// The seek-versus-scan comparison is only meaningful if the two
/// repositories hold the same graph, so pin that rather than trusting the
/// generator to be deterministic across two calls (`acetone-2ck.16`).
#[test]
fn the_unindexed_twin_holds_an_identical_graph() {
    let shape = acetone_lab::Shape::from_scale(200);

    let indexed_dir = tempfile::tempdir().expect("tempdir");
    let indexed =
        Repository::init(&indexed_dir.path().join("a.git"), InitOptions::default()).expect("init");
    let with_indexes = acetone_lab::build_with(&indexed, shape, true).expect("build");

    let plain_dir = tempfile::tempdir().expect("tempdir");
    let plain =
        Repository::init(&plain_dir.path().join("b.git"), InitOptions::default()).expect("init");
    let without_indexes = acetone_lab::build_with(&plain, shape, false).expect("build");

    assert_eq!(
        with_indexes, without_indexes,
        "the twins must hold identical node and edge counts"
    );

    // Identical CONTENT, not merely identical counts. Counts are the weak
    // check twice over: `build_with` derives its return value from the
    // `Shape` and a seeded counter without reading the repository, and even
    // a snapshot-derived count cannot see a divergence that preserves
    // cardinality — a property value differing between the two builds, or a
    // record written under a different encoding.
    //
    // The map roots are the exact check: identical map contents yield
    // identical prolly-tree roots regardless of operation order (load-bearing
    // invariant 1). So equal roots mean equal graphs, and declaring an index
    // must not perturb the node or edge maps at all.
    //
    // Checked through `twins_match`, the same function the criterion-3
    // harness calls, so this test constrains the real check rather than a
    // copy of it.
    let a = indexed.workspace_snapshot().expect("snapshot");
    let b = plain.workspace_snapshot().expect("snapshot");
    let (ma, mb) = (a.manifest(), b.manifest());
    assert_eq!(
        acetone_lab::twins_match(ma, mb),
        Ok(()),
        "declaring an index perturbed the node or edge maps"
    );
    // The schema roots MUST differ — that is the one intended difference,
    // and equal schema roots would mean the twin was built with indexes
    // after all, making every measured ratio a comparison of like with like.
    assert_ne!(
        ma.schema, mb.schema,
        "the twins must differ in their declared schema"
    );

    // And the twin genuinely has no indexes to seek with.
    use acetone_model::schema::SchemaEntry;
    assert!(
        !b.schema_entries()
            .expect("schema")
            .iter()
            .any(|e| matches!(e, SchemaEntry::Index { .. })),
        "the unindexed twin must declare no indexes"
    );
    assert!(
        a.schema_entries()
            .expect("schema")
            .iter()
            .any(|e| matches!(e, SchemaEntry::Index { .. })),
        "the indexed side must declare indexes"
    );
}

/// `twins_match` is only worth having if it can FAIL. The assertion it
/// replaced could not: it compared `build_with`'s return value, which is
/// derived from the `Shape` and a seeded counter without reading the
/// repository, so it held whatever the two repositories actually contained.
///
/// This calls `twins_match` itself — the same function the criterion-3
/// harness calls — and requires it to reject a pair that is not a twin. A
/// refactor that pointed it at fields equal for both repositories would fail
/// here, which is the property the test above cannot establish on its own
/// (it can only show the function accepts a true twin, which a function that
/// accepts everything also does).
#[test]
fn twins_match_rejects_a_divergent_graph() {
    // Only the SHAPE varies. Both sides declare indexes, so a difference can
    // only come from the graph's content — otherwise this could not tell
    // "roots track content" from "roots track index declaration".
    let roots = |scale: usize| {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo =
            Repository::init(&dir.path().join("r.git"), InitOptions::default()).expect("init");
        acetone_lab::build_with(&repo, acetone_lab::Shape::from_scale(scale), true).expect("build");
        let snapshot = repo.workspace_snapshot().expect("snapshot");
        // `Manifest` is not `Copy`; clone it out before the tempdir goes.
        let manifest = snapshot.manifest().clone();
        drop(snapshot);
        drop(dir);
        manifest
    };

    let reference = roots(200);
    let divergent = roots(300);

    let rejected = acetone_lab::twins_match(&reference, &divergent)
        .expect_err("twins_match accepted two differently-shaped graphs as twins");
    // Name the maps that diverged, so a failing run says which.
    assert!(
        rejected.contains("nodes") && rejected.contains("edges_fwd"),
        "the rejection must name the diverging maps, got: {rejected}"
    );
    // And it still accepts a genuine twin, so the rejection above is not
    // simply a function that refuses everything.
    assert_eq!(acetone_lab::twins_match(&reference, &reference), Ok(()));
}
