//! End-to-end proof of the value-domain round-trip carrier (ADR-0038,
//! `acetone-vdc`): a `Bytes`/temporal property that is *read but not touched* by
//! a write query must survive on disk as its original type, for **both nodes
//! and edges** — closing the loss ADR-0029 patched for nodes only.
//!
//! These drive the real write path (`execute_write` → `persist_changes` →
//! `Transaction::save` → re-read) against an on-disk repository, which the
//! in-crate `GraphSource` unit tests cannot reach. v0.1 Cypher cannot construct
//! a `Bytes`/temporal literal, so the deferred value is seeded through the graph
//! layer; the carrier is what makes it observable and preservable through a
//! query.

use std::collections::BTreeMap;

use acetone_cypher::bind::{BindMode, bind};
use acetone_cypher::exec::value::Value as RtValue;
use acetone_cypher::exec::{GraphSnapshot, SingleVersion, catalogue_from_schema, execute_write};
use acetone_cypher::persist::persist_changes;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, RelTypeDef, SchemaEntry};
use acetone_model::{DateTime, Value as MV};

/// A schema with one label `N` keyed on `id` (so persist can derive identity).
fn node_schema() -> SchemaEntry {
    SchemaEntry::Label {
        name: "N".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label def"),
    }
}

fn node_key(id: i64) -> NodeKey {
    NodeKey::new("N", vec![MV::Int(id)]).expect("node key")
}

/// A DateTime value distinct from any string a query could accidentally match.
fn sample_datetime() -> MV {
    MV::DateTime(DateTime {
        epoch_nanos: 1_600_000_000_000_000_000,
        offset_minutes: 60,
    })
}

/// Run `query` as a write against `repo`'s workspace, replaying its net changes
/// and saving — the same wiring the CLI's `run_write` uses.
fn run_write(repo: &Repository, query: &str) {
    let mut txn = repo.begin_write().expect("begin write");
    let snapshot = repo.workspace_snapshot().expect("workspace snapshot");
    let nodes = snapshot.nodes().expect("nodes");
    let edges = snapshot.edges().expect("edges");
    let schema = snapshot.schema_entries().expect("schema");

    let base = GraphSnapshot::from_records_with_schema(&nodes, &edges, &schema);
    let catalogue = catalogue_from_schema(schema);
    let parsed = acetone_cypher::parse(query).expect("parse");
    let bound = bind(query, &parsed, &catalogue, BindMode::Strict).expect("bind");
    let resolver = SingleVersion::new(&base);
    let (_result, changes) =
        execute_write(&bound, &resolver, &BTreeMap::new()).expect("execute write");

    persist_changes(&changes, &mut txn, &catalogue, &snapshot).expect("persist");
    txn.save().expect("save");
}

#[test]
fn unchanged_node_bytes_and_temporal_survive_a_set_as_their_types() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");

    let data = MV::Bytes(vec![0xde, 0xad, 0xbe, 0xef]);
    let when = sample_datetime();

    // Seed a node bearing a Bytes and a DateTime property via the graph layer.
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&node_schema()).expect("schema");
        txn.put_node(
            &node_key(1),
            &NodeRecord::new(
                [],
                BTreeMap::from([
                    ("name".to_owned(), MV::String("old".into())),
                    ("data".to_owned(), data.clone()),
                    ("when".to_owned(), when.clone()),
                ]),
            ),
        )
        .expect("put node");
        txn.save().expect("save seed");
    }

    // Touch only `name`. The read adapter carries `data`/`when` as Value::Stored,
    // the executor leaves them untouched, and persist writes them back verbatim.
    run_write(&repo, "MATCH (n:N {id: 1}) SET n.name = 'new' RETURN n.id");

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let record = snapshot
        .get_node(&node_key(1))
        .expect("read")
        .expect("node present");
    assert_eq!(
        record.properties().get("name"),
        Some(&MV::String("new".into())),
        "the touched property is updated"
    );
    assert_eq!(
        record.properties().get("data"),
        Some(&data),
        "untouched Bytes must survive as Bytes, not a hex string"
    );
    assert_eq!(
        record.properties().get("when"),
        Some(&when),
        "untouched DateTime must survive as DateTime, not a debug string"
    );
}

#[test]
fn unchanged_edge_bytes_survives_a_set_as_its_type() {
    // The ADR-0029 gap: edge write-back threaded no base record, so an untouched
    // deferred edge property was retyped to a string. The carrier closes it.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");

    let payload = MV::Bytes(vec![0x01, 0x02, 0x03, 0x04]);
    let stamp = sample_datetime();

    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&node_schema()).expect("schema");
        txn.put_schema(&SchemaEntry::RelType {
            name: "LINK".into(),
            def: RelTypeDef::new(None, BTreeMap::new(), []).expect("rtype"),
        })
        .expect("rel schema");
        txn.put_node(&node_key(1), &NodeRecord::new([], BTreeMap::new()))
            .expect("node a");
        txn.put_node(&node_key(2), &NodeRecord::new([], BTreeMap::new()))
            .expect("node b");
        let edge = EdgeKey::new(node_key(1), "LINK", node_key(2), MV::Null).expect("edge key");
        txn.put_edge(
            &edge,
            &EdgeRecord::new(BTreeMap::from([
                ("payload".to_owned(), payload.clone()),
                ("stamp".to_owned(), stamp.clone()),
            ])),
        )
        .expect("put edge");
        txn.save().expect("save seed");
    }

    // Touch only `tag` on the edge; `payload` (Bytes) and `stamp` (DateTime) are
    // read (as carriers) and written back untouched.
    run_write(
        &repo,
        "MATCH (a:N {id: 1})-[r:LINK]->(b:N {id: 2}) SET r.tag = 'x' RETURN a.id",
    );

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let edges = snapshot.edges().expect("edges");
    let (_key, record) = edges
        .iter()
        .find(|(k, _)| k.rtype() == "LINK")
        .expect("edge present");
    assert_eq!(
        record.properties().get("tag"),
        Some(&MV::String("x".into())),
        "the touched edge property is updated"
    );
    assert_eq!(
        record.properties().get("payload"),
        Some(&payload),
        "untouched edge Bytes must survive as Bytes, not a hex string (the ADR-0029 gap)"
    );
    assert_eq!(
        record.properties().get("stamp"),
        Some(&stamp),
        "untouched edge DateTime must survive as DateTime, not a debug string"
    );
}

#[test]
fn setting_a_deferred_property_to_its_own_rendering_stores_a_string() {
    // The headline ADR-0029 false-positive, fixed by ADR-0038: read a Bytes
    // property, then `SET` it to a genuine string literal that happens to equal
    // its own hex rendering. The old re-read heuristic would wrongly resurrect
    // the typed Bytes; the carrier stores the string the user actually wrote,
    // because a user `SET` yields a `Value::String`, never a `Value::Stored`.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&node_schema()).expect("schema");
        txn.put_node(
            &node_key(1),
            &NodeRecord::new(
                [],
                BTreeMap::from([("data".to_owned(), MV::Bytes(vec![0xde, 0xad, 0xbe, 0xef]))]),
            ),
        )
        .expect("put node");
        txn.save().expect("save");
    }

    run_write(
        &repo,
        "MATCH (n:N {id: 1}) SET n.data = 'deadbeef' RETURN n.id",
    );

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let record = snapshot
        .get_node(&node_key(1))
        .expect("read")
        .expect("node present");
    assert_eq!(
        record.properties().get("data"),
        Some(&MV::String("deadbeef".into())),
        "a user SET to a string literal stores a string, not the resurrected Bytes"
    );
}

#[test]
fn a_carrier_decays_to_its_string_through_a_function() {
    // A deferred value consumed by a string function is rendered exactly as the
    // pre-carrier runtime saw it (ADR-0038 behavioural equivalence): reading a
    // Bytes property through `toUpper` yields the upper-cased hex, not an error.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&node_schema()).expect("schema");
        txn.put_node(
            &node_key(1),
            &NodeRecord::new(
                [],
                BTreeMap::from([("data".to_owned(), MV::Bytes(vec![0xab, 0xcd]))]),
            ),
        )
        .expect("put node");
        txn.save().expect("save");
    }

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let nodes = snapshot.nodes().expect("nodes");
    let edges = snapshot.edges().expect("edges");
    let schema = snapshot.schema_entries().expect("schema");
    let base = GraphSnapshot::from_records_with_schema(&nodes, &edges, &schema);
    let catalogue = catalogue_from_schema(schema);
    let query = "MATCH (n:N {id: 1}) RETURN toUpper(n.data)";
    let parsed = acetone_cypher::parse(query).expect("parse");
    let bound = bind(query, &parsed, &catalogue, BindMode::Strict).expect("bind");
    let resolver = SingleVersion::new(&base);
    let (result, _changes) = execute_write(&bound, &resolver, &BTreeMap::new()).expect("execute");

    assert!(
        matches!(&result.rows[0][0], RtValue::String(s) if s == "ABCD"),
        "a Bytes property decays to its hex string for `toUpper`, got {:?}",
        result.rows[0][0]
    );
}
