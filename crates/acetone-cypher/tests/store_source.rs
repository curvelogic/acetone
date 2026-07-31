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
    // `id` key (string), `region` typed String (seek-safe), `port` UNTYPED.
    //
    // `port` is untyped because the composite tests below seed it with both
    // `Int(80)` and `Float(80.0)` to exercise per-component numeric
    // cross-typing on the stored map, and since ADR-0066 made declared types
    // enforced at save, a property declared `int` can no longer hold a float.
    //
    // Untyped is NOT the only state that would work: `float` admits an
    // integer (ADR-0066's one widening), so declaring `port: float` would
    // keep both values legal AND keep a declared type on the component. This
    // fixture covers a genuinely reachable state, but it is a larger
    // loosening than enforcement required, and it leaves no composite fixture
    // with a declared type at a non-zero component — so the positive arm of
    // `probe_value`'s per-component guard is uncovered. `acetone-2ck.20`.
    SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(
            vec!["id".into()],
            BTreeMap::from([("region".to_owned(), PropertyType::String)]),
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

/// PR #219 review blocker 3: a `Bytes`/temporal key value compares equal
/// to its string *rendering* at runtime, but the stored encodings differ.
/// A string pin probing only the string encoding would MISS the
/// deferred-typed node — under-selection, which candidate-superset
/// semantics forbid — so the key seek declines string pins and lets the
/// scan answer. Two spellings of one predicate must never disagree.
///
/// This is the **undeclared** case, which stays declined after
/// `acetone-2ck.17`: with no declared type on the key property there is
/// nothing to rule the hazard out.
#[test]
fn string_key_pin_declines_rather_than_under_selecting() {
    let (_dir, repo1) = repo();
    {
        let mut txn = repo1.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "Host".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        txn.put_node(
            &NodeKey::new("Host", vec![MV::String("deadbeef".into())]).expect("key"),
            &NodeRecord::new([], BTreeMap::from([("tag".to_owned(), MV::Int(1))])),
        )
        .expect("string-keyed node");
        txn.save().expect("save");
    }
    let snapshot = repo1.workspace_snapshot().expect("snapshot");
    let schema = snapshot.schema_entries().expect("schema");
    let source = StoreBackedSource::new(&snapshot, &schema);

    assert!(
        source
            .nodes_by_key("Host", &[RtValue::String("deadbeef".into())])
            .is_none(),
        "a string pin must decline (scan) rather than serve a possibly \
         under-selecting probe set"
    );
    // Numeric keys are unaffected — those encodings cannot collide with a
    // deferred value's rendering.
    let (_dir2, repo2) = repo();
    {
        let mut txn = repo2.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        txn.put_node(
            &NodeKey::new("N", vec![MV::Int(5)]).expect("key"),
            &NodeRecord::new([], BTreeMap::new()),
        )
        .expect("node");
        txn.save().expect("save");
    }
    let snap2 = repo2.workspace_snapshot().expect("snapshot");
    let schema2 = snap2.schema_entries().expect("schema");
    let source2 = StoreBackedSource::new(&snap2, &schema2);
    assert_eq!(
        source2
            .nodes_by_key("N", &[RtValue::Int(5)])
            .expect("numeric pin is served")
            .len(),
        1
    );
}

/// Build a one-label repository and return a snapshot over it.
fn repo_with_label(
    label: &str,
    key: Vec<String>,
    types: BTreeMap<String, PropertyType>,
    nodes: &[(Vec<MV>, i64)],
) -> (tempfile::TempDir, Repository) {
    let (dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: label.into(),
            def: LabelDef::new(key, types, [], []).expect("label"),
        })
        .expect("schema");
        for (key_tuple, tag) in nodes {
            txn.put_node(
                &NodeKey::new(label, key_tuple.clone()).expect("key"),
                &NodeRecord::new([], BTreeMap::from([("tag".to_owned(), MV::Int(*tag))])),
            )
            .expect("node");
        }
        txn.save().expect("save");
    }
    (dir, repo)
}

/// The rows a scan would return for `label` where the `component`-th key
/// property equals `pin` — computed the way the executor's filter does,
/// through `eq3`, so a `Stored` carrier decays to its string rendering.
fn scan_rows(source: &StoreBackedSource<'_>, key_name: &str, pin: &RtValue) -> Vec<String> {
    let mut out: Vec<String> = source
        .all_nodes()
        .into_iter()
        .filter(|n| {
            n.properties
                .get(key_name)
                .is_some_and(|v| v.eq3(pin) == Some(true))
        })
        .map(|n| format!("{:?}", n.properties.get("tag")))
        .collect();
    out.sort();
    out
}

/// `acetone-2ck.17`: a primary-key point lookup is the most valuable seek
/// there is, and it was scanning for every string key — 257.13 ms indexed
/// vs 257.69 ms unindexed over 50,000 hosts, i.e. 1.00x.
///
/// The declared type is what rules the rendering hazard out, exactly as it
/// does for an equality pin in `probe_value`. ADR-0066 made a declaration a
/// constraint enforced at save, declare and merge time — including for key
/// properties, whose values live in the key tuple rather than the record —
/// so trusting it here is trusting something checked.
#[test]
fn a_declared_string_key_is_seekable_and_agrees_with_the_scan() {
    let (_dir, repo) = repo_with_label(
        "Host",
        vec!["id".into()],
        BTreeMap::from([("id".to_owned(), PropertyType::String)]),
        &[
            (vec![MV::String("h1".into())], 1),
            (vec![MV::String("h2".into())], 2),
            (vec![MV::String("h3".into())], 3),
        ],
    );
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let source = source_over(&snapshot);

    let pin = RtValue::String("h2".into());
    let served = source
        .nodes_by_key("Host", std::slice::from_ref(&pin))
        .expect("a declared-string key pin must be SERVED, not declined");
    let mut seek_rows: Vec<String> = served
        .iter()
        .map(|n| format!("{:?}", n.properties.get("tag")))
        .collect();
    seek_rows.sort();
    assert_eq!(
        seek_rows,
        scan_rows(&source, "id", &pin),
        "the seek must return exactly the scan's rows"
    );
    assert_eq!(seek_rows.len(), 1, "and that is one node, not zero");

    // A pin that matches nothing still declines rather than asserting
    // absence — unchanged, and the reason `served` is meaningful above.
    assert!(
        source
            .nodes_by_key("Host", &[RtValue::String("absent".into())])
            .is_none()
    );
}

/// The hazard, in the only shape that actually produces a wrong answer.
///
/// A lone `Bytes`-keyed node is *not* enough: a key probe that finds
/// nothing returns `None` and the scan answers, so an under-selecting probe
/// degrades safely. The wrong answer needs a **collision** — two nodes
/// under one label whose key values are distinct when stored (`String` vs
/// `Bytes` encodings) but equal at runtime, because the `Bytes` value's
/// ADR-0038 carrier decays to the same hex rendering under `eq3`. A raw
/// string probe then finds the first, declares itself served, and silently
/// drops the second.
///
/// The key property is undeclared here, which is what makes the collision
/// reachable at all: since ADR-0066 a declared type is enforced, so a label
/// declaring `id: string` cannot hold the `Bytes`-keyed twin, and that is
/// precisely why the declaration is what the guard trusts.
#[test]
fn an_undeclared_string_key_pin_would_drop_a_colliding_row() {
    let (_dir, repo) = repo_with_label(
        "Blob",
        vec!["id".into()],
        BTreeMap::new(),
        &[
            (vec![MV::String("deadbeef".into())], 1),
            (vec![MV::Bytes(vec![0xde, 0xad, 0xbe, 0xef])], 2),
        ],
    );
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let source = source_over(&snapshot);

    let pin = RtValue::String("deadbeef".into());
    assert_eq!(
        scan_rows(&source, "id", &pin).len(),
        2,
        "both nodes match the pin at runtime — this is the collision"
    );
    assert!(
        source
            .nodes_by_key("Blob", std::slice::from_ref(&pin))
            .is_none(),
        "so the seek must decline: serving the string probe alone would \
         return one row where the scan returns two — under-selection, which \
         candidate-superset semantics forbid"
    );
}

/// The allow-list, pinned **behaviourally** — a `bytes`-declared key must
/// decline a string pin, and the proof is a state where declining changes
/// the answer.
///
/// An earlier version of this test used a lone `Bytes`-keyed node and was
/// vacuous: with the guard forced open the probe builds
/// `NodeKey("Blob", [String("deadbeef")])`, finds nothing, leaves `served`
/// false, and returns `None` regardless. Only a **collision** distinguishes
/// the two — a `String`-keyed and a `Bytes`-keyed node under one label,
/// distinct in the nodes map, equal at runtime.
///
/// The collision is stored under an **untyped** label — which is legal, and
/// how a graph written before ADR-0066 carries one — and the declaration is
/// supplied through the source's own schema argument, which is
/// `StoreBackedSource`'s actual contract: it is handed a snapshot and a
/// schema. That is the state a pre-enforcement repository presents, and one
/// a `migrate::FormatTransform` can still write.
///
/// It deliberately does **not** use a conflicted merge to reach the
/// declaration. A conflicted workspace is the other route, but since
/// `acetone-7qw.14` the source stops trusting declarations there, so the
/// guard would decline because the type reads as *absent* — proving nothing
/// about the allow-list. That is the same vacuity trap as the test this one
/// replaced; the distinguishing state has to be one where types are trusted.
#[test]
fn a_bytes_declared_key_declines_where_declining_changes_the_answer() {
    let (_dir, repo) = repo();
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Blob".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("untyped"),
    })
    .expect("schema");
    for (key, tag) in [
        (MV::Bytes(vec![0xde, 0xad, 0xbe, 0xef]), 1),
        (MV::String("deadbeef".into()), 2),
    ] {
        txn.put_node(
            &NodeKey::new("Blob", vec![key]).expect("key"),
            &NodeRecord::new([], BTreeMap::from([("tag".to_owned(), MV::Int(tag))])),
        )
        .expect("node");
    }
    txn.save().expect("save");

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    // The declaration the stored data contradicts, supplied as the source's
    // schema — no plumbing, and no conflicted workspace.
    let declared = [SchemaEntry::Label {
        name: "Blob".into(),
        def: LabelDef::new(
            vec!["id".into()],
            BTreeMap::from([("id".to_owned(), PropertyType::Bytes)]),
            [],
            [],
        )
        .expect("typed"),
    }];
    let source = StoreBackedSource::new(&snapshot, &declared);

    let pin = RtValue::String("deadbeef".into());
    assert_eq!(
        scan_rows(&source, "id", &pin).len(),
        2,
        "both nodes match the pin at runtime — the collision is present"
    );
    assert!(
        source
            .nodes_by_key("Blob", std::slice::from_ref(&pin))
            .is_none(),
        "a bytes-declared key must decline: serving the string probe would \
         find one node and silently drop the other"
    );
}

/// `acetone-7qw.14`: a merge that produces graph-level violations persists
/// the merged manifest as the workspace and rebuilds its indexes over the
/// merged nodes **before** `validate_merged` runs. Commit is refused, so
/// nothing reaches history — but that workspace is live and queryable, and
/// in it a declaration can be false of the data beside it.
///
/// Every write-time check is by design powerless here: conflicts are data,
/// not errors (ADR-0007), so the state is legitimate. It is the **reader**
/// that must stop trusting a declaration it can see is contested. Without
/// that, `MATCH (b:Blob {id: 'deadbeef'})` returns one row where the scan
/// spelling returns two — a silent short answer, on node identity, at
/// precisely the moment a user is querying to decide how to resolve.
///
/// Reached from three ordinary commits, each legal on its own.
#[test]
fn a_conflicted_workspace_does_not_trust_its_own_declarations() {
    let (_dir, repo) = repo();
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Blob".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("untyped"),
    })
    .expect("schema");
    txn.put_node(
        &NodeKey::new("Blob", vec![MV::String("deadbeef".into())]).expect("key"),
        &NodeRecord::new([], BTreeMap::from([("tag".to_owned(), MV::Int(1))])),
    )
    .expect("string-keyed");
    let base = txn.commit("base", &[], None).expect("commit");

    // The branch declares `id: string`. Legal: its own data conforms.
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Blob".into(),
        def: LabelDef::new(
            vec!["id".into()],
            BTreeMap::from([("id".to_owned(), PropertyType::String)]),
            [],
            [],
        )
        .expect("typed"),
    })
    .expect("schema");
    txn.commit("declare id: string", &[], None).expect("commit");

    // `main` adds a Bytes-keyed twin. Legal: still untyped over here.
    repo.checkout_branch("main").expect("checkout");
    let mut txn = repo.begin_write().expect("begin");
    txn.put_node(
        &NodeKey::new("Blob", vec![MV::Bytes(vec![0xde, 0xad, 0xbe, 0xef])]).expect("key"),
        &NodeRecord::new([], BTreeMap::from([("tag".to_owned(), MV::Int(2))])),
    )
    .expect("bytes-keyed");
    txn.commit("add a bytes-keyed twin", &[], None)
        .expect("commit");

    repo.merge("other", "merge").expect("merge");

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    assert!(
        snapshot.manifest().conflicts.is_some(),
        "the fixture must actually be a conflicted workspace"
    );
    let source = source_over(&snapshot);
    let pin = RtValue::String("deadbeef".into());
    assert_eq!(
        scan_rows(&source, "id", &pin).len(),
        2,
        "both nodes match the pin at runtime"
    );
    assert!(
        source
            .nodes_by_key("Blob", std::slice::from_ref(&pin))
            .is_none(),
        "the seek must decline rather than answer from a declaration the \
         merge itself has flagged as contested — serving it returns one row \
         where the scan returns two"
    );
}

/// The other half of `acetone-7qw.14`: distrust is scoped to the conflicted
/// workspace. An ordinary repository must still seek, or the fix is a silent
/// performance regression that turns every string key lookup back into a
/// scan — the very thing `acetone-2ck.17` removed.
#[test]
fn an_unconflicted_workspace_still_trusts_its_declarations() {
    let (_dir, repo) = repo_with_label(
        "Host",
        vec!["id".into()],
        BTreeMap::from([("id".to_owned(), PropertyType::String)]),
        &[(vec![MV::String("h1".into())], 1)],
    );
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    assert!(snapshot.manifest().conflicts.is_none());
    let source = source_over(&snapshot);
    assert_eq!(
        source
            .nodes_by_key("Host", &[RtValue::String("h1".into())])
            .expect("a declared string key must still be SERVED outside a merge")
            .len(),
        1
    );
}

/// Per component, like `probe_value`: a composite key is seekable only when
/// every string component's declared type rules the hazard out. One
/// undeclared component is enough to send the whole pin to a scan.
#[test]
fn a_composite_key_needs_every_string_component_declared() {
    let (_dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "Both".into(),
            def: LabelDef::new(
                vec!["a".into(), "b".into()],
                BTreeMap::from([
                    ("a".to_owned(), PropertyType::String),
                    ("b".to_owned(), PropertyType::String),
                ]),
                [],
                [],
            )
            .expect("label"),
        })
        .expect("schema");
        txn.put_schema(&SchemaEntry::Label {
            name: "Half".into(),
            def: LabelDef::new(
                vec!["a".into(), "b".into()],
                BTreeMap::from([("a".to_owned(), PropertyType::String)]),
                [],
                [],
            )
            .expect("label"),
        })
        .expect("schema");
        for label in ["Both", "Half"] {
            txn.put_node(
                &NodeKey::new(label, vec![MV::String("x".into()), MV::String("y".into())])
                    .expect("key"),
                &NodeRecord::new([], BTreeMap::new()),
            )
            .expect("node");
        }
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let source = source_over(&snapshot);

    let pin = [RtValue::String("x".into()), RtValue::String("y".into())];
    assert_eq!(
        source
            .nodes_by_key("Both", &pin)
            .expect("both components declared — served")
            .len(),
        1
    );
    assert!(
        source.nodes_by_key("Half", &pin).is_none(),
        "an undeclared string component sends the whole composite pin to a scan"
    );
}

/// acetone-2ck.14: `IndexRange` must be served by the STORE-backed source
/// — the shipped read path — and must agree with a scan exactly. Range
/// bounds live inside the index key's nested value list, so this is the
/// parity assertion that matters: over-selection is safe (a later filter
/// re-checks), under-selection is a wrong answer.
#[test]
fn index_range_is_served_by_the_store_and_agrees_with_a_scan() {
    let (_dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "M".into(),
            def: LabelDef::new(
                vec!["id".into()],
                BTreeMap::from([("v".to_owned(), PropertyType::Int)]),
                [],
                [],
            )
            .expect("label"),
        })
        .expect("schema");
        txn.put_schema(&SchemaEntry::Index {
            name: "m_v".into(),
            def: IndexDef::new("M", vec!["v".into()]).expect("index"),
        })
        .expect("index");
        for id in 0..60i64 {
            txn.put_node(
                &NodeKey::new("M", vec![MV::Int(id)]).expect("key"),
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), MV::Int(id))])),
            )
            .expect("node");
        }
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let schema = snapshot.schema_entries().expect("schema");
    let source = StoreBackedSource::new(&snapshot, &schema);

    // Every bound shape, checked against the truth a scan would give.
    type Bound = Option<(RtValue, bool)>;
    let cases: Vec<(Bound, Bound)> = vec![
        (None, Some((RtValue::Int(30), false))), // v < 30
        (None, Some((RtValue::Int(30), true))),  // v <= 30
        (Some((RtValue::Int(50), false)), None), // v > 50
        (Some((RtValue::Int(50), true)), None),  // v >= 50
        (
            Some((RtValue::Int(10), true)),
            Some((RtValue::Int(20), false)),
        ),
        (
            Some((RtValue::Int(20), false)),
            Some((RtValue::Int(10), true)),
        ), // inverted
        (
            Some((RtValue::Int(-5), true)),
            Some((RtValue::Int(3), true)),
        ), // spans zero
        (Some((RtValue::Int(0), true)), Some((RtValue::Int(0), true))), // single point
    ];
    for (lo, hi) in cases {
        let lo_ref = lo.as_ref().map(|(v, i)| (v, *i));
        let hi_ref = hi.as_ref().map(|(v, i)| (v, *i));
        let served = source
            .nodes_by_index_range("m_v", "v", lo_ref, hi_ref)
            .unwrap_or_else(|| panic!("store must serve the range {lo:?}..{hi:?}"));

        let expected: Vec<i64> = (0..60i64)
            .filter(|n| match &lo {
                Some((RtValue::Int(b), true)) => n >= b,
                Some((RtValue::Int(b), false)) => n > b,
                _ => true,
            })
            .filter(|n| match &hi {
                Some((RtValue::Int(b), true)) => n <= b,
                Some((RtValue::Int(b), false)) => n < b,
                _ => true,
            })
            .collect();
        let mut got: Vec<i64> = served
            .iter()
            .map(|node| match node.properties.get("v") {
                Some(RtValue::Int(n)) => *n,
                other => panic!("expected int, got {other:?}"),
            })
            .collect();
        got.sort_unstable();
        // Over-selection is permitted (a candidate superset); missing a
        // node the scan would find is not.
        for want in &expected {
            assert!(
                got.contains(want),
                "range {lo:?}..{hi:?} UNDER-SELECTED: missing {want} (got {got:?})"
            );
        }
        assert!(
            got.iter().all(|n| expected.contains(n)),
            "range {lo:?}..{hi:?} returned rows outside it: {got:?} vs {expected:?}"
        );
    }

    // A float bound over an int column still agrees (3 == 3.0).
    let served = source
        .nodes_by_index_range("m_v", "v", None, Some((&RtValue::Float(5.5), false)))
        .expect("float bound is served");
    assert_eq!(served.len(), 6, "v < 5.5 selects 0..=5");
}

/// PR #221 review finding 6: the parity test above uses only `Int`
/// values, so it never exercised the deferred-typed hazard the range
/// code's central comment reasons about, nor floats, extremes, or nodes
/// missing the property. This one stores every value kind in one indexed
/// column and asserts the seek never misses a row the predicate accepts.
#[test]
fn index_range_over_a_mixed_type_column_never_under_selects() {
    let (_dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "X".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        txn.put_schema(&SchemaEntry::Index {
            name: "x_v".into(),
            def: IndexDef::new("X", vec!["v".into()]).expect("index"),
        })
        .expect("index");
        // Every kind that can sit in a property, including ones the index
        // is blind to (null/NaN) and ones the runtime compares as their
        // string rendering (Bytes, temporal).
        let values: Vec<Option<MV>> = vec![
            Some(MV::Int(i64::MIN)),
            Some(MV::Int(-1)),
            Some(MV::Int(0)),
            Some(MV::Int(1)),
            Some(MV::Int(9_007_199_254_740_992)), // 2^53
            Some(MV::Int(i64::MAX)),
            Some(MV::Float(-0.0)),
            Some(MV::Float(0.0)),
            Some(MV::Float(0.5)),
            Some(MV::Float(f64::MIN_POSITIVE)),
            Some(MV::Float(f64::INFINITY)),
            Some(MV::Float(f64::NEG_INFINITY)),
            Some(MV::String("0".into())),
            Some(MV::String("zzz".into())),
            Some(MV::Bool(true)),
            Some(MV::Bytes(vec![0, 1, 2])),
            Some(MV::Null),
            None, // property absent entirely
        ];
        for (i, v) in values.iter().enumerate() {
            let mut props = BTreeMap::new();
            if let Some(v) = v {
                props.insert("v".to_owned(), v.clone());
            }
            txn.put_node(
                &NodeKey::new("X", vec![MV::Int(i as i64)]).expect("key"),
                &NodeRecord::new([], props),
            )
            .expect("node");
        }
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let schema = snapshot.schema_entries().expect("schema");
    let source = StoreBackedSource::new(&snapshot, &schema);

    // Numeric bounds over the mixed column. Truth is whatever the scan
    // would keep: only values that compare as numbers can satisfy these.
    let all = source.all_nodes();
    for (lo, hi) in [
        (None, Some((RtValue::Int(1), false))),
        (None, Some((RtValue::Int(0), true))),
        (Some((RtValue::Int(0), true)), None),
        (
            Some((RtValue::Int(-1), false)),
            Some((RtValue::Int(2), true)),
        ),
        (
            Some((RtValue::Float(-0.5), true)),
            Some((RtValue::Float(0.5), true)),
        ),
    ] {
        let lo_ref = lo.as_ref().map(|(v, i)| (v, *i));
        let hi_ref = hi.as_ref().map(|(v, i)| (v, *i));
        let Some(served) = source.nodes_by_index_range("x_v", "v", lo_ref, hi_ref) else {
            continue; // declined — the scan answers, always correct
        };
        // Truth: every node whose value the predicate accepts under
        // openCypher comparison (a string/bytes/bool/null value never
        // compares less/greater than a number, so it is excluded).
        let want: Vec<&RtValue> = all
            .iter()
            .filter_map(|n| n.properties.get("v"))
            .filter(|v| {
                let num = matches!(v, RtValue::Int(_) | RtValue::Float(_));
                if !num {
                    return false;
                }
                let as_f = match v {
                    RtValue::Int(n) => *n as f64,
                    RtValue::Float(f) => *f,
                    _ => unreachable!(),
                };
                if as_f.is_nan() {
                    return false;
                }
                let lo_ok = match &lo {
                    Some((RtValue::Int(b), true)) => as_f >= *b as f64,
                    Some((RtValue::Int(b), false)) => as_f > *b as f64,
                    Some((RtValue::Float(b), true)) => as_f >= *b,
                    Some((RtValue::Float(b), false)) => as_f > *b,
                    _ => true,
                };
                let hi_ok = match &hi {
                    Some((RtValue::Int(b), true)) => as_f <= *b as f64,
                    Some((RtValue::Int(b), false)) => as_f < *b as f64,
                    Some((RtValue::Float(b), true)) => as_f <= *b,
                    Some((RtValue::Float(b), false)) => as_f < *b,
                    _ => true,
                };
                lo_ok && hi_ok
            })
            .collect();
        for w in want {
            assert!(
                served.iter().any(|n| n
                    .properties
                    .get("v")
                    .is_some_and(|v| format!("{v:?}") == format!("{w:?}"))),
                "range {lo:?}..{hi:?} UNDER-SELECTED: missing {w:?}"
            );
        }
    }

    // A non-numeric bound declines outright rather than risk missing a
    // Bytes/temporal value the runtime would compare as a string.
    assert!(
        source
            .nodes_by_index_range(
                "x_v",
                "v",
                None,
                Some((&RtValue::String("m".into()), false))
            )
            .is_none(),
        "a string bound must decline"
    );
}

/// acetone-2ck.2: the seek must decline when it would lose to a scan.
/// A seek does one random point read per row where the scan reads
/// sequentially, so firing on an unselective probe made a declared index
/// make queries *slower* — 3.7× at the lab's own composite ratio, 18× at
/// 20% selectivity. Both seek paths now size a budget from the index's
/// height and hand unselective probes back to the scan.
#[test]
fn unselective_seeks_decline_on_both_paths() {
    let (_dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "W".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        txn.put_schema(&SchemaEntry::Index {
            name: "w_b".into(),
            def: IndexDef::new("W", vec!["b".into()]).expect("index"),
        })
        .expect("index");
        // 4000 rows in one bucket (unselective) and one row in another
        // (selective), so a single fixture exercises both directions.
        for i in 0..4000i64 {
            txn.put_node(
                &NodeKey::new("W", vec![MV::Int(i)]).expect("key"),
                &NodeRecord::new([], BTreeMap::from([("b".to_owned(), MV::Int(0))])),
            )
            .expect("node");
        }
        txn.put_node(
            &NodeKey::new("W", vec![MV::Int(9999)]).expect("key"),
            &NodeRecord::new([], BTreeMap::from([("b".to_owned(), MV::Int(7))])),
        )
        .expect("node");
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let schema = snapshot.schema_entries().expect("schema");
    let source = StoreBackedSource::new(&snapshot, &schema);

    // Equality: the 4000-row bucket exceeds the budget for this index's
    // height, so the seek declines rather than paying 4000 point reads.
    assert!(
        source
            .nodes_by_index("w_b", &["b".to_owned()], &[&RtValue::Int(0)])
            .is_none(),
        "an unselective equality probe must decline, not fire and lose"
    );
    // The selective bucket is still served.
    let served = source
        .nodes_by_index("w_b", &["b".to_owned()], &[&RtValue::Int(7)])
        .expect("a selective probe is served");
    assert_eq!(served.len(), 1);
    // An absent value is served too — proving absence is the cheapest
    // thing a seek does, and the one lab row that survives the store.
    assert_eq!(
        source
            .nodes_by_index("w_b", &["b".to_owned()], &[&RtValue::Int(4242)])
            .expect("an absent probe is served")
            .len(),
        0
    );

    // Range: the same budget governs, so a range covering everything
    // declines while a narrow one serves.
    assert!(
        source
            .nodes_by_index_range("w_b", "b", None, Some((&RtValue::Int(100), false)))
            .is_none(),
        "an unselective range must decline"
    );
    assert_eq!(
        source
            .nodes_by_index_range(
                "w_b",
                "b",
                Some((&RtValue::Int(6), false)),
                Some((&RtValue::Int(8), false))
            )
            .expect("a selective range is served")
            .len(),
        1
    );
}

/// An over-budget probe declines outright on both paths, including on a
/// shape where the two numeric family probes overlap.
///
/// This guards a hazard rather than a demonstrated bug. Probes are
/// de-duplicated across numeric families and composite tuples, so a walk
/// that stopped early can in principle yield FEWER distinct keys than the
/// cap; were "over budget" inferred from `len > cap`, the caller would read
/// that as a complete walk and return the short set — under-selection, a
/// wrong answer rather than a slow one. `Candidates::OverBudget` makes the
/// two states distinct so the inference cannot be made. No query was found
/// that reaches the bad case today (the first family trips the budget
/// first), so this test pins the declining behaviour, not the near miss.
#[test]
fn a_truncated_walk_is_never_mistaken_for_a_complete_one() {
    let (_dir, repo) = repo();
    let rows = 4_000i64;
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "D".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        txn.put_schema(&SchemaEntry::Index {
            name: "d_v".into(),
            def: IndexDef::new("D", vec!["v".into()]).expect("index"),
        })
        .expect("index");
        for i in 0..rows {
            txn.put_node(
                &NodeKey::new("D", vec![MV::Int(i)]).expect("key"),
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), MV::Float(1.0))])),
            )
            .expect("node");
        }
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let schema = snapshot.schema_entries().expect("schema");
    let source = StoreBackedSource::new(&snapshot, &schema);

    // An unselective range over a column whose int and float probes both
    // reach every row: decline outright, never a partial answer.
    let got = source.nodes_by_index_range(
        "d_v",
        "v",
        Some((&RtValue::Int(0), true)),
        Some((&RtValue::Int(2), true)),
    );
    assert!(
        got.is_none(),
        "an over-budget range must decline, not return {} of {rows} rows",
        got.map_or(0, |g| g.len())
    );

    // And the equality path, whose int/float probes are separate tuples.
    let got = source.nodes_by_index("d_v", &["v".to_owned()], &[&RtValue::Int(1)]);
    assert!(
        got.is_none(),
        "an over-budget equality must decline, not return {} of {rows} rows",
        got.map_or(0, |g| g.len())
    );
}

/// The budget is a fraction of what a scan would cost, sampled from the
/// nodes map rather than tiered on the index's height — height changes
/// once per fanout, so one tier spans ~10× in cardinality and cannot be
/// calibrated to both ends (PR #224 review blocker 1).
#[test]
fn candidate_budget_tracks_estimated_scan_cost() {
    use acetone_cypher::exec::store_source::candidate_cap;

    // 0.5% of the rows a scan would visit. Measured break-even is 0.28-0.61%
    // on the raw primitives and nearer 1% end to end (2% ran 1.9x SLOWER on
    // a small-record shape — PR #224 review finding 4), and the cardinality
    // estimate feeding this is itself accurate only to ~1.4x in the
    // dangerous direction. Half of break-even absorbs that.
    assert_eq!(candidate_cap(1_000_000), 5_000);
    assert_eq!(candidate_cap(50_000), 250, "the measured ~50k anchor");
    assert_eq!(candidate_cap(10_000), 50);

    // A floor, so point-lookup shapes still work on tiny graphs — where a
    // scan is cheap anyway, so a wrong choice costs little.
    assert_eq!(
        candidate_cap(1_100),
        32,
        "small labels get the floor, not 1024"
    );
    assert_eq!(candidate_cap(0), 32);

    assert!(
        candidate_cap(200_000) > candidate_cap(50_000),
        "monotone in the scan cost it is competing with"
    );

    // A saturated estimate must not panic. `estimate_entries` multiplies
    // mean fanouts as an `f64` and casts with `as usize`, which saturates,
    // so a pathological tree can present usize::MAX here — and this runs in
    // debug in every test and CI build (PR #224 review finding 1).
    assert_eq!(candidate_cap(usize::MAX), usize::MAX / 200);
}

/// acetone-7qw.9: an index must be usable from the form people actually
/// write. Before this, `MATCH (n:H {b: 3})` used the index while
/// `MATCH (n:H) WHERE n.b = 3` scanned — ranges in WHERE attached hints,
/// equality did not, so the hint had no values to seek with.
#[test]
fn where_equality_uses_the_index() {
    use acetone_cypher::bind::bound::IndexHint;
    use acetone_cypher::{bind, parse};

    let catalogue = {
        let entries = vec![
            SchemaEntry::Label {
                name: "H".into(),
                def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
            },
            SchemaEntry::Index {
                name: "h_b".into(),
                def: IndexDef::new("H", vec!["b".into()]).expect("index"),
            },
        ];
        acetone_cypher::exec::catalogue_from_schema(entries)
    };
    let hint_for = |q: &str| {
        let parsed = parse(q).expect("parse");
        let bound = bind::bind(q, &parsed, &catalogue, bind::BindMode::Strict).expect("bind");
        for clause in &bound.clauses {
            if let acetone_cypher::bind::bound::BoundClause::Match { patterns, .. } = clause {
                return patterns[0].start.index_hints.first().cloned();
            }
        }
        None
    };

    // The WHERE form now attaches an IndexSeek carrying its own value.
    match hint_for("MATCH (n:H) WHERE n.b = 3 RETURN n") {
        Some(IndexHint::IndexSeek { name, values, .. }) => {
            assert_eq!(name, "h_b");
            assert!(values.is_some(), "the hint must carry the WHERE's value");
        }
        other => panic!("expected an IndexSeek from WHERE, got {other:?}"),
    }
    // Reversed operand order too.
    assert!(matches!(
        hint_for("MATCH (n:H) WHERE 3 = n.b RETURN n"),
        Some(IndexHint::IndexSeek { .. })
    ));
    // A key pin in WHERE becomes a KeySeek, as it does inline.
    match hint_for("MATCH (n:H) WHERE n.id = 7 RETURN n") {
        Some(IndexHint::KeySeek { key, values, .. }) => {
            assert_eq!(key, vec!["id".to_string()]);
            assert!(values.is_some());
        }
        other => panic!("expected a KeySeek from WHERE, got {other:?}"),
    }
    // The inline form is unchanged and still reads from the pattern map.
    match hint_for("MATCH (n:H {b: 3}) RETURN n") {
        Some(IndexHint::IndexSeek { values, .. }) => {
            assert!(values.is_none(), "inline pins still read the pattern map")
        }
        other => panic!("expected an inline IndexSeek, got {other:?}"),
    }
    // A non-constant comparison pins nothing.
    assert!(hint_for("MATCH (n:H) WHERE n.b = n.id RETURN n").is_none());
    // OR is not a pin.
    assert!(hint_for("MATCH (n:H) WHERE n.b = 3 OR n.b = 4 RETURN n").is_none());
}

/// PR #224 review blocker 2: hints are ordered CANDIDATES. An equality
/// hint that declines at runtime must fall through to a range hint that
/// would serve — before this, the binder attached only one hint, so a
/// declining equality discarded a far more selective range and the query
/// ran 80–91× slower than with no hint at all.
#[test]
fn a_declining_hint_falls_through_to_the_next() {
    use acetone_cypher::bind::bound::IndexHint;
    use acetone_cypher::{bind, parse};

    let entries = vec![
        SchemaEntry::Label {
            name: "H".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        },
        SchemaEntry::Index {
            name: "h_b".into(),
            def: IndexDef::new("H", vec!["b".into()]).expect("index"),
        },
        SchemaEntry::Index {
            name: "h_u".into(),
            def: IndexDef::new("H", vec!["u".into()]).expect("index"),
        },
    ];
    let catalogue = acetone_cypher::exec::catalogue_from_schema(entries);
    let q = "MATCH (n:H) WHERE n.b = 0 AND n.u > 49990 RETURN n";
    let parsed = parse(q).expect("parse");
    let bound = bind::bind(q, &parsed, &catalogue, bind::BindMode::Strict).expect("bind");
    let acetone_cypher::bind::bound::BoundClause::Match { patterns, .. } = &bound.clauses[0] else {
        panic!("expected a MATCH")
    };
    let hints = &patterns[0].start.index_hints;
    assert_eq!(
        hints.len(),
        2,
        "both the equality and the range must be offered: {hints:?}"
    );
    assert!(
        matches!(hints[0], IndexHint::IndexSeek { .. }),
        "equality is tried first"
    );
    assert!(
        matches!(hints[1], IndexHint::IndexRange { .. }),
        "the range remains available when the equality declines"
    );
}

/// PR #224 review finding 6: nothing in the suite exercised the cap
/// boundary itself. A result of `cap` or fewer entries must be a COMPLETE
/// walk — serving a truncated set would be silent under-selection — and
/// `cap + 1` must decline. Pinned on both paths at the exact edges.
#[test]
fn the_cap_boundary_is_exact() {
    use acetone_cypher::exec::store_source::candidate_cap;

    // Build a graph whose bucket sizes straddle the cap for its own size.
    let (_dir, repo) = repo();
    let rows = 4_000i64;
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "B".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        txn.put_schema(&SchemaEntry::Index {
            name: "b_v".into(),
            def: IndexDef::new("B", vec!["v".into()]).expect("index"),
        })
        .expect("index");
        for i in 0..rows {
            // v = 0 for a big bucket; v = 1 for a single row.
            let v = if i == 0 { 1 } else { 0 };
            txn.put_node(
                &NodeKey::new("B", vec![MV::Int(i)]).expect("key"),
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), MV::Int(v))])),
            )
            .expect("node");
        }
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let estimate = snapshot.estimate_nodes().expect("estimate");
    let cap = candidate_cap(estimate);
    let schema = snapshot.schema_entries().expect("schema");
    let source = StoreBackedSource::new(&snapshot, &schema);

    // The big bucket is (rows - 1); assert the fixture actually straddles
    // the cap, so this test cannot silently stop testing the boundary.
    assert!(
        (rows as usize - 1) > cap,
        "fixture must exceed the cap ({} rows vs cap {cap}, estimate {estimate})",
        rows - 1
    );

    // The primitive's contract AT the boundary, since the end-to-end
    // assertions below straddle it by thousands and would not catch an
    // off-by-one (PR #224 review finding 5). `index_scan_capped` stops at
    // cap + 1, so "cap or fewer" must mean a COMPLETE walk — that is the
    // property the decline logic rests on.
    let bucket = (rows - 1) as usize;
    let prefix =
        acetone_model::graph_keys::index_value_prefix("B", &["v".to_owned()], &[MV::Int(0)])
            .expect("prefix");
    for (cap_under_test, expected) in [
        (bucket - 2, bucket - 1), // truncated at cap + 1
        (bucket - 1, bucket),     // the last cap that still signals decline
        (bucket, bucket),         // exactly the bucket: complete
        (usize::MAX, bucket),     // uncapped: complete
    ] {
        let got = snapshot
            .index_scan_capped("b_v", &prefix, cap_under_test)
            .expect("scan")
            .expect("index exists");
        assert_eq!(
            got.len(),
            expected,
            "cap {cap_under_test} over a {bucket}-entry bucket"
        );
        // The contract the decline logic rests on, as an implication: a
        // result within the cap is a COMPLETE walk. (The converse does not
        // hold — a complete walk of exactly cap + 1 entries reports as
        // over-budget, which is conservative and therefore safe.)
        if got.len() <= cap_under_test {
            assert_eq!(
                got.len(),
                bucket,
                "'cap or fewer' must mean a complete walk, at cap {cap_under_test}"
            );
        }
    }
    assert!(
        source
            .nodes_by_index("b_v", &["v".to_owned()], &[&RtValue::Int(0)])
            .is_none(),
        "a bucket past the cap declines"
    );
    // Under the cap: served, and COMPLETE — a truncated serve would be a
    // wrong answer, so compare against the scan's own count.
    let served = source
        .nodes_by_index("b_v", &["v".to_owned()], &[&RtValue::Int(1)])
        .expect("a one-row bucket is served");
    let truth = source
        .all_nodes()
        .iter()
        .filter(|n| matches!(n.properties.get("v"), Some(RtValue::Int(1))))
        .count();
    assert_eq!(served.len(), truth, "a served set must be complete");
}
