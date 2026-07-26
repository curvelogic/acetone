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
        .nodes_by_index(
            "host_region",
            &["region".into()],
            &[&RtValue::String("eu".into())],
        )
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
        .nodes_by_index("host_port", &["port".into()], &[&RtValue::Int(80)])
        .expect("served");
    assert_eq!(
        names(by_int.iter().map(id_of).collect()),
        vec!["a".to_string(), "c".to_string()]
    );

    // Float pin 80.0 must select the same nodes (3 = 3.0 cross-type).
    let by_float = src
        .nodes_by_index("host_port", &["port".into()], &[&RtValue::Float(80.0)])
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
        src.nodes_by_index(
            "thing_tag",
            &["tag".into()],
            &[&RtValue::String("x".into())]
        )
        .is_none(),
        "a string pin on an untyped index property must fall back to a scan"
    );
    // A numeric pin is still safe even when untyped (never matches a rendering).
    assert!(
        src.nodes_by_index("thing_tag", &["tag".into()], &[&RtValue::Int(1)])
            .is_some()
    );
}

#[test]
fn unknown_index_falls_back() {
    let (_d, repo) = repo();
    seed(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);
    assert!(
        src.nodes_by_index("no_such", &["p".into()], &[&RtValue::Int(1)])
            .is_none()
    );
}

#[test]
fn expand_reads_incident_edges_lazily() {
    let (_d, repo) = repo();
    seed(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);

    let a = src.node_by_id_via_index("host_region", "region", "eu", "a");

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
    let b = src.node_by_id_via_index("host_region", "region", "eu", "b");
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
    let a = src.node_by_id_via_index("host_region", "region", "eu", "a");
    let node = src.node(&a).expect("node present");
    assert_eq!(id_of(&node), "a");
    assert!(src.take_error().is_none());
}

/// Test helper: fetch a specific node's `EntityId` through the index seek.
trait FindNode {
    fn node_by_id_via_index(
        &self,
        index: &str,
        property: &str,
        value: &str,
        id: &str,
    ) -> acetone_cypher::exec::value::EntityId;
}
impl FindNode for StoreBackedSource<'_> {
    fn node_by_id_via_index(
        &self,
        index: &str,
        property: &str,
        value: &str,
        id: &str,
    ) -> acetone_cypher::exec::value::EntityId {
        self.nodes_by_index(
            index,
            &[property.to_owned()],
            &[&RtValue::String(value.into())],
        )
        .expect("served")
        .into_iter()
        .find(|n| id_of(n) == id)
        .expect("node present")
        .id
    }
}

// --- Composite index seeks (acetone-0c7, PR #207 review Major) --------------

/// A repo with a composite `(region, port)` index over Hosts whose `port`
/// values mix Int and Float, plus an `Item` label carrying an UNTYPED
/// `tag` inside a composite — the typed-string safety must apply per
/// component.
fn seed_composite(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&host_label()).expect("label");
    tx.put_schema(&SchemaEntry::Index {
        name: "host_region_port".into(),
        def: IndexDef::new("Host", vec!["region".into(), "port".into()]).expect("idx"),
    })
    .expect("composite idx");
    // Item: `id` key; `region` typed String; `tag` UNTYPED.
    tx.put_schema(&SchemaEntry::Label {
        name: "Item".into(),
        def: LabelDef::new(
            vec!["id".into()],
            BTreeMap::from([("region".to_owned(), PropertyType::String)]),
            [],
            [],
        )
        .expect("label"),
    })
    .expect("item label");
    tx.put_schema(&SchemaEntry::Index {
        name: "item_region_tag".into(),
        def: IndexDef::new("Item", vec!["region".into(), "tag".into()]).expect("idx"),
    })
    .expect("item idx");
    for (id, region, port) in [
        ("a", "eu", MV::Int(80)),
        ("b", "eu", MV::Float(80.0)),
        ("c", "eu", MV::Int(443)),
        ("d", "us", MV::Int(80)),
    ] {
        tx.put_node(
            &node(id),
            &NodeRecord::new(
                [],
                BTreeMap::from([
                    ("region".to_owned(), MV::String(region.into())),
                    ("port".to_owned(), port),
                ]),
            ),
        )
        .expect("node");
    }
    tx.put_node(
        &NodeKey::new("Item", vec![MV::String("i1".into())]).expect("key"),
        &NodeRecord::new(
            [],
            BTreeMap::from([
                ("region".to_owned(), MV::String("eu".into())),
                ("tag".to_owned(), MV::String("x".into())),
            ]),
        ),
    )
    .expect("item");
    tx.save().expect("save");
}

#[test]
fn composite_seek_serves_with_per_component_cross_typing() {
    let (_d, repo) = repo();
    seed_composite(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);
    let props: Vec<String> = vec!["region".into(), "port".into()];
    // An Int pin reaches BOTH the Int(80) and Float(80.0) entries under
    // ("eu", 80) — per-component cross-typing on the stored map.
    let region = RtValue::String("eu".into());
    let port = RtValue::Int(80);
    let got = src
        .nodes_by_index("host_region_port", &props, &[&region, &port])
        .expect("served");
    assert_eq!(names(got.iter().map(id_of).collect()), vec!["a", "b"]);
    // The float orientation reaches the same pair.
    let port = RtValue::Float(80.0);
    let got = src
        .nodes_by_index("host_region_port", &props, &[&region, &port])
        .expect("served");
    assert_eq!(got.len(), 2);
    // Arity / property-list mismatches refuse (scan fallback).
    assert!(
        src.nodes_by_index("host_region_port", &["region".into()], &[&region])
            .is_none()
    );
    let wrong: Vec<String> = vec!["port".into(), "region".into()];
    assert!(
        src.nodes_by_index("host_region_port", &wrong, &[&region, &region])
            .is_none()
    );
}

#[test]
fn composite_seek_edges_bail_or_empty_per_component() {
    let (_d, repo) = repo();
    seed_composite(&repo);
    let snap = repo.workspace_snapshot().expect("snap");
    let src = source_over(&snap);
    let props: Vec<String> = vec!["region".into(), "port".into()];
    let region = RtValue::String("eu".into());
    // An integral float >= 2^53 in a NON-FIRST component bails the whole
    // seek (non-unique i64 preimage — under-selection hazard).
    let edge = RtValue::Float(9_007_199_254_740_992.0);
    assert!(
        src.nodes_by_index("host_region_port", &props, &[&region, &edge])
            .is_none()
    );
    // A null component selects nothing, definitively (null-blind index).
    let null = RtValue::Null;
    let got = src
        .nodes_by_index("host_region_port", &props, &[&region, &null])
        .expect("served");
    assert!(got.is_empty());
    // A string pin on the UNTYPED second component refuses: a stored
    // Bytes/temporal value's rendering would be missed by a raw probe.
    let item_props: Vec<String> = vec!["region".into(), "tag".into()];
    let tag = RtValue::String("x".into());
    assert!(
        src.nodes_by_index("item_region_tag", &item_props, &[&region, &tag])
            .is_none()
    );
}

/// Phase 9 security review, finding 7: `StoreBackedSource` implemented
/// only `nodes_by_index`, so a primary-key pin fell through to the
/// `GraphSource` default (`None`) and always label-scanned — the seek
/// existed but was unreachable through `Session`, the shipped read path.
#[test]
fn primary_key_pin_is_served_by_the_store_backed_source() {
    let (_dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "Host".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        for id in 0..50i64 {
            txn.put_node(
                &NodeKey::new("Host", vec![MV::Int(id)]).expect("key"),
                &NodeRecord::new([], BTreeMap::from([("n".to_owned(), MV::Int(id * 10))])),
            )
            .expect("node");
        }
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let schema = snapshot.schema_entries().expect("schema");
    let source = StoreBackedSource::new(&snapshot, &schema);

    // The key pin is served (Some), not declined (None -> scan).
    let served = source
        .nodes_by_key("Host", &[RtValue::Int(7)])
        .expect("primary key pin must be served by the store");
    assert_eq!(served.len(), 1, "exactly the pinned node");
    assert!(
        matches!(served[0].properties.get("n"), Some(RtValue::Int(70))),
        "the right node"
    );

    // 7.0 finds the same node: openCypher equates 3 and 3.0, and the two
    // encode differently, so both numeric encodings are probed.
    let by_float = source
        .nodes_by_key("Host", &[RtValue::Float(7.0)])
        .expect("integral float pin serves too");
    assert_eq!(by_float.len(), 1);

    // A pin the seek cannot serve declines (scan), never asserts absence.
    assert!(
        source.nodes_by_key("Host", &[RtValue::Null]).is_none(),
        "a null pin declines rather than claiming nothing matches"
    );
}
