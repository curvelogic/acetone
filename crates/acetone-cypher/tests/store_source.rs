//! The lazy store-backed GraphSource (ADR-0040, `acetone-cbl.11`): correctness
//! against a real on-disk `Snapshot` — an index seek returns exactly the scan's
//! rows, numeric cross-type and the raw-vs-rendered fallback behave, and
//! `expand`/`node` read incident edges and point records lazily.

use std::collections::BTreeMap;

use acetone_cypher::ast::Direction;
use acetone_cypher::exec::source::GraphSource;
use acetone_cypher::exec::store_source::StoreBackedSource;
use acetone_cypher::exec::value::Value as RtValue;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value as MV;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{IndexDef, LabelDef, PropertyType, SchemaEntry};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    (dir, repo)
}

fn host_label() -> SchemaEntry {
    // `id` key (string), `region` typed String (seek-safe), `port` typed Int.
    SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(
            vec!["id".into()],
            BTreeMap::from([
                ("region".to_owned(), PropertyType::String),
                ("port".to_owned(), PropertyType::Int),
            ]),
            [],
            [],
        )
        .expect("label"),
    }
}

fn node(id: &str) -> NodeKey {
    NodeKey::new("Host", vec![MV::String(id.into())]).expect("key")
}

/// A single-labelled query source over the workspace and its schema.
fn source_over<'s>(snapshot: &'s acetone_graph::repo::Snapshot<'s>) -> StoreBackedSource<'s> {
    let schema = snapshot.schema_entries().expect("schema");
    StoreBackedSource::new(snapshot, &schema)
}

fn names(mut nodes: Vec<String>) -> Vec<String> {
    nodes.sort();
    nodes
}

fn id_of(node: &acetone_cypher::exec::value::NodeValue) -> String {
    match node.properties.get("id") {
        Some(RtValue::String(s)) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn seed(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&host_label()).expect("label");
    tx.put_schema(&SchemaEntry::Index {
        name: "host_region".into(),
        def: IndexDef::new("Host", vec!["region".into()]).expect("idx"),
    })
    .expect("region idx");
    tx.put_schema(&SchemaEntry::Index {
        name: "host_port".into(),
        def: IndexDef::new("Host", vec!["port".into()]).expect("idx"),
    })
    .expect("port idx");
    tx.put_schema(&SchemaEntry::RelType {
        name: "LINK".into(),
        def: acetone_model::schema::RelTypeDef::new(None, BTreeMap::new(), []).expect("rtype"),
    })
    .expect("rtype");
    for (id, region, port) in [("a", "eu", 80), ("b", "eu", 443), ("c", "us", 80)] {
        tx.put_node(
            &node(id),
            &NodeRecord::new(
                [],
                BTreeMap::from([
                    ("region".to_owned(), MV::String(region.into())),
                    ("port".to_owned(), MV::Int(port)),
                ]),
            ),
        )
        .expect("node");
    }
    // a -> b, a -> c (LINK)
    for (src, dst) in [("a", "b"), ("a", "c")] {
        tx.put_edge(
            &EdgeKey::new(node(src), "LINK", node(dst), MV::Null).expect("edge"),
            &EdgeRecord::new(BTreeMap::from([("w".to_owned(), MV::Int(1))])),
        )
        .expect("edge");
    }
    tx.save().expect("save");
}

#[test]
fn string_index_seek_matches_the_scan() {
    let (_d, repo) = repo();
    seed(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);

    // A String pin on a String-typed property is served by the seek.
    let got = src
        .nodes_by_index("host_region", &RtValue::String("eu".into()))
        .expect("seek served");
    assert_eq!(
        names(got.iter().map(id_of).collect()),
        vec!["a".to_string(), "b".to_string()]
    );
    assert!(src.take_error().is_none());

    // Agreement with a label scan filtered by the same predicate.
    let scan: Vec<String> = src
        .nodes_by_labels(&["Host".to_string()])
        .into_iter()
        .filter(|n| matches!(n.properties.get("region"), Some(RtValue::String(s)) if s == "eu"))
        .map(|n| id_of(&n))
        .collect();
    assert_eq!(names(scan), vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn numeric_index_seek_probes_int_and_float() {
    let (_d, repo) = repo();
    seed(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);

    // Int pin: matches the Int-stored port 80 on a and c.
    let by_int = src
        .nodes_by_index("host_port", &RtValue::Int(80))
        .expect("served");
    assert_eq!(
        names(by_int.iter().map(id_of).collect()),
        vec!["a".to_string(), "c".to_string()]
    );

    // Float pin 80.0 must select the same nodes (3 = 3.0 cross-type).
    let by_float = src
        .nodes_by_index("host_port", &RtValue::Float(80.0))
        .expect("served");
    assert_eq!(
        names(by_float.iter().map(id_of).collect()),
        vec!["a".to_string(), "c".to_string()]
    );
    assert!(src.take_error().is_none());
}

#[test]
fn a_string_pin_on_an_untyped_property_falls_back_to_a_scan() {
    // An index whose property has no declared type could hold a Bytes/temporal
    // value (keyed raw) that a string pin would match by rendering — so a raw
    // probe is unsafe and the seek must return None (scan fallback).
    let (_d, repo) = repo();
    {
        let mut tx = repo.begin_write().expect("begin");
        // Label with a key but NO declared type for `tag`.
        tx.put_schema(&SchemaEntry::Label {
            name: "Thing".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("label");
        tx.put_schema(&SchemaEntry::Index {
            name: "thing_tag".into(),
            def: IndexDef::new("Thing", vec!["tag".into()]).expect("idx"),
        })
        .expect("idx");
        tx.put_node(
            &NodeKey::new("Thing", vec![MV::Int(1)]).expect("k"),
            &NodeRecord::new(
                [],
                BTreeMap::from([("tag".to_owned(), MV::String("x".into()))]),
            ),
        )
        .expect("node");
        tx.save().expect("save");
    }
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);
    assert!(
        src.nodes_by_index("thing_tag", &RtValue::String("x".into()))
            .is_none(),
        "a string pin on an untyped index property must fall back to a scan"
    );
    // A numeric pin is still safe even when untyped (never matches a rendering).
    assert!(src.nodes_by_index("thing_tag", &RtValue::Int(1)).is_some());
}

#[test]
fn unknown_index_falls_back() {
    let (_d, repo) = repo();
    seed(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);
    assert!(src.nodes_by_index("no_such", &RtValue::Int(1)).is_none());
}

#[test]
fn expand_reads_incident_edges_lazily() {
    let (_d, repo) = repo();
    seed(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);

    let a = src.node_by_id_via_index("host_region", "eu", "a");

    // Out: a -> b, a -> c.
    let mut out: Vec<String> = src
        .expand(&a, Direction::Out, &[])
        .into_iter()
        .map(|(_, n)| id_of(&n))
        .collect();
    out.sort();
    assert_eq!(out, vec!["b".to_string(), "c".to_string()]);

    // Type filter: only LINK (all of them) — a non-existent type yields nothing.
    assert!(
        src.expand(&a, Direction::Out, &["NOPE".to_string()])
            .is_empty()
    );

    // In-edges of b: a -> b.
    let b = src.node_by_id_via_index("host_region", "eu", "b");
    let into: Vec<String> = src
        .expand(&b, Direction::In, &[])
        .into_iter()
        .map(|(_, n)| id_of(&n))
        .collect();
    assert_eq!(into, vec!["a".to_string()]);
    assert!(src.take_error().is_none());
}

#[test]
fn node_round_trips_by_id() {
    let (_d, repo) = repo();
    seed(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);
    let a = src.node_by_id_via_index("host_region", "eu", "a");
    let node = src.node(&a).expect("node present");
    assert_eq!(id_of(&node), "a");
    assert!(src.take_error().is_none());
}

/// Test helper: fetch a specific node's `EntityId` through the index seek.
trait FindNode {
    fn node_by_id_via_index(
        &self,
        index: &str,
        value: &str,
        id: &str,
    ) -> acetone_cypher::exec::value::EntityId;
}
impl FindNode for StoreBackedSource<'_> {
    fn node_by_id_via_index(
        &self,
        index: &str,
        value: &str,
        id: &str,
    ) -> acetone_cypher::exec::value::EntityId {
        self.nodes_by_index(index, &RtValue::String(value.into()))
            .expect("served")
            .into_iter()
            .find(|n| id_of(n) == id)
            .expect("node present")
            .id
    }
}
