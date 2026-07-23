//! `SET x = e` / `SET x += e` where `e` is a node or relationship
//! (bead acetone-q9m): openCypher copies the source entity's property
//! map, exactly as if a map literal had been written. Surfaced by TCK
//! Merge6 [6] and Merge7 [4] (`... ON CREATE SET r = a` / `ON MATCH SET
//! r = a`), which acetone rejected with "SET = needs a map, got Node".
//!
//! These drive the same schema-free path the TCK write harness uses:
//! lenient bind against an empty catalogue, `execute_write` over a
//! `MemoryGraph`, changes applied between statements.

use std::collections::BTreeMap;

use acetone_cypher::bind::{BindMode, Catalogue, bind};
use acetone_cypher::exec::value::Value;
use acetone_cypher::exec::{GraphSource, MemoryGraph, SingleVersion, execute_write};

/// Parse, bind (leniently, schema-free) and execute one statement against
/// `graph`, applying its changes.
fn run(graph: &mut MemoryGraph, query: &str) -> acetone_cypher::exec::QueryResult {
    let parsed = acetone_cypher::parse(query).expect("parse");
    let bound = bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient).expect("bind");
    let (result, changes) = {
        let resolver = SingleVersion::new(&*graph);
        execute_write(&bound, &resolver, &BTreeMap::new()).expect("execute")
    };
    graph.apply(&changes);
    result
}

/// The property map of the single relationship in `graph`.
fn only_rel_props(graph: &MemoryGraph) -> BTreeMap<String, Value> {
    let rels = graph.all_rels();
    assert_eq!(rels.len(), 1, "expected exactly one relationship");
    rels[0].properties.clone()
}

fn as_str(value: &Value) -> &str {
    match value {
        Value::String(s) => s,
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn set_replace_from_node_copies_its_properties_and_drops_the_rest() {
    let mut graph = MemoryGraph::new();
    run(&mut graph, "CREATE (:A {name: 'A'}), (:B {name: 'B'})");
    run(
        &mut graph,
        "MATCH (a:A), (b:B) CREATE (a)-[:T {name: 'bar', old: 1}]->(b)",
    );

    // `SET r = a` replaces the relationship's whole property map with the
    // node's — `old` must vanish, `name` must become the node's value.
    run(&mut graph, "MATCH (a:A) MATCH ()-[r:T]->() SET r = a");

    let props = only_rel_props(&graph);
    assert_eq!(
        props.len(),
        1,
        "replace drops unlisted properties: {props:?}"
    );
    assert_eq!(as_str(&props["name"]), "A");
}

#[test]
fn set_merge_from_node_keeps_unlisted_properties() {
    let mut graph = MemoryGraph::new();
    run(&mut graph, "CREATE (:A {name: 'A'}), (:B {name: 'B'})");
    run(
        &mut graph,
        "MATCH (a:A), (b:B) CREATE (a)-[:T {name: 'bar', old: 1}]->(b)",
    );

    run(&mut graph, "MATCH (a:A) MATCH ()-[r:T]->() SET r += a");

    let props = only_rel_props(&graph);
    assert_eq!(props.len(), 2, "merge keeps unlisted properties: {props:?}");
    assert_eq!(as_str(&props["name"]), "A");
    assert_eq!(
        format!("{:?}", props["old"]),
        format!("{:?}", Value::Int(1))
    );
}

#[test]
fn set_replace_from_relationship_copies_onto_a_node() {
    let mut graph = MemoryGraph::new();
    run(&mut graph, "CREATE (:A {stale: true}), (:B)");
    run(
        &mut graph,
        "MATCH (a:A), (b:B) CREATE (a)-[:T {name: 'rel'}]->(b)",
    );

    run(&mut graph, "MATCH (a:A)-[r:T]->() SET a = r");

    let nodes = graph.all_nodes();
    let a = nodes
        .iter()
        .find(|n| n.labels.contains(&"A".to_string()))
        .expect("node :A");
    assert_eq!(
        a.properties.len(),
        1,
        "replace drops `stale`: {:?}",
        a.properties
    );
    assert_eq!(as_str(&a.properties["name"]), "rel");
}

#[test]
fn merge_on_create_set_from_node_copies_properties() {
    // The exact shape of TCK Merge6 [6].
    let mut graph = MemoryGraph::new();
    run(&mut graph, "CREATE (:A {name: 'A'}), (:B {name: 'B'})");
    run(
        &mut graph,
        "MATCH (a {name: 'A'}), (b {name: 'B'}) MERGE (a)-[r:TYPE]->(b) ON CREATE SET r = a",
    );

    let props = only_rel_props(&graph);
    assert_eq!(props.len(), 1);
    assert_eq!(as_str(&props["name"]), "A");
}

#[test]
fn set_replace_from_a_non_entity_is_still_a_type_error() {
    let mut graph = MemoryGraph::new();
    run(&mut graph, "CREATE (:A)");

    let query = "MATCH (a:A) SET a = 1";
    let parsed = acetone_cypher::parse(query).expect("parse");
    let bound = bind(query, &parsed, &Catalogue::empty(), BindMode::Lenient).expect("bind");
    let resolver = SingleVersion::new(&graph);
    let err = execute_write(&bound, &resolver, &BTreeMap::new()).expect_err("SET a = 1 must fail");
    let message = err.to_string();
    assert!(
        message.contains("needs a map"),
        "expected the map type error, got: {message}"
    );
}
