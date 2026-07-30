//! Declared property types enforced at the **transaction** level (ADR-0066),
//! including the case a schema-only transaction used to slip past entirely.
//!
//! `check_staged_node_types` judges what a transaction *writes*. That leaves
//! the other direction — moving the declaration onto data that is already
//! there — and `Transaction::put_schema` is public, so the CLI's
//! `declare-label` backfill check does not cover it. Retyping around existing
//! data made a declaration false the moment it landed, and both seek paths
//! decide a raw string probe is exact from the declaration alone.

use acetone_graph::GraphError;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{LabelDef, PropertyType, SchemaEntry};
use std::collections::BTreeMap;

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    (dir, repo)
}

fn label(key: &str, types: BTreeMap<String, PropertyType>) -> SchemaEntry {
    SchemaEntry::Label {
        name: "Blob".into(),
        def: LabelDef::new(vec![key.to_owned()], types, [], []).expect("label"),
    }
}

/// The reachable wrong answer this guards. Two nodes whose key values are
/// distinct when stored (`String` vs `Bytes` encodings) but equal at runtime
/// — a `Bytes` carrier renders to the same hex under `eq3`. Declaring the key
/// `string` afterwards would make a primary-key seek probe only the string
/// encoding, returning one row where a scan returns two: under-selection, on
/// node identity, in a repository built entirely through the public library.
#[test]
fn retyping_a_key_around_existing_data_is_refused() {
    let (_dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&label("id", BTreeMap::new()))
            .expect("schema");
        txn.put_node(
            &NodeKey::new("Blob", vec![Value::String("deadbeef".into())]).expect("key"),
            &NodeRecord::new([], BTreeMap::new()),
        )
        .expect("string-keyed");
        txn.put_node(
            &NodeKey::new("Blob", vec![Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef])]).expect("key"),
            &NodeRecord::new([], BTreeMap::new()),
        )
        .expect("bytes-keyed");
        txn.save().expect("save");
    }

    // A schema-only transaction: nothing is staged in the nodes map, so the
    // write-time check sees nothing at all.
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&label(
        "id",
        BTreeMap::from([("id".to_owned(), PropertyType::String)]),
    ))
    .expect("stage schema");
    let err = txn
        .save()
        .expect_err("declaring id:string over a Bytes key must be refused");
    assert!(
        matches!(err, GraphError::PropertyTypeViolation { ref property, declared, actual, .. }
            if property == "id" && declared == "string" && actual == "bytes"),
        "expected a PropertyTypeViolation naming the key property, got: {err}"
    );
}

/// The same rule for an ordinary record property.
#[test]
fn retyping_a_record_property_around_existing_data_is_refused() {
    let (_dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&label("id", BTreeMap::new()))
            .expect("schema");
        txn.put_node(
            &NodeKey::new("Blob", vec![Value::String("a".into())]).expect("key"),
            &NodeRecord::new(
                [],
                BTreeMap::from([("size".to_owned(), Value::String("8".into()))]),
            ),
        )
        .expect("node");
        txn.save().expect("save");
    }
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&label(
        "id",
        BTreeMap::from([("size".to_owned(), PropertyType::Int)]),
    ))
    .expect("stage schema");
    assert!(
        matches!(
            txn.save(),
            Err(GraphError::PropertyTypeViolation { ref property, .. }) if property == "size"
        ),
        "declaring size:int over a string value must be refused"
    );
}

/// Refusing the *state a transaction is in the middle of repairing* would be
/// a bad rule: a retype and its backfill must be able to land together. Nodes
/// the transaction itself rewrites are the write-time check's business.
#[test]
fn a_retype_and_its_backfill_land_in_one_transaction() {
    let (_dir, repo) = repo();
    let key = NodeKey::new("Blob", vec![Value::String("a".into())]).expect("key");
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&label("id", BTreeMap::new()))
            .expect("schema");
        txn.put_node(
            &key,
            &NodeRecord::new(
                [],
                BTreeMap::from([("size".to_owned(), Value::String("8".into()))]),
            ),
        )
        .expect("node");
        txn.save().expect("save");
    }
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&label(
        "id",
        BTreeMap::from([("size".to_owned(), PropertyType::Int)]),
    ))
    .expect("stage schema");
    txn.put_node(
        &key,
        &NodeRecord::new([], BTreeMap::from([("size".to_owned(), Value::Int(8))])),
    )
    .expect("backfill");
    txn.save()
        .expect("retype plus backfill in one transaction must succeed");

    // And the repaired value is what landed.
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let record = snapshot.get_node(&key).expect("read").expect("present");
    assert_eq!(record.properties().get("size"), Some(&Value::Int(8)));
}

/// Only the properties whose declaration actually changed are re-judged, so
/// an unrelated schema write does not re-litigate settled data — the same
/// responsibility rule the merge path uses.
#[test]
fn an_unrelated_schema_change_does_not_re_judge_existing_data() {
    let (_dir, repo) = repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&label(
            "id",
            BTreeMap::from([("size".to_owned(), PropertyType::Int)]),
        ))
        .expect("schema");
        txn.put_node(
            &NodeKey::new("Blob", vec![Value::String("a".into())]).expect("key"),
            &NodeRecord::new([], BTreeMap::from([("size".to_owned(), Value::Int(8))])),
        )
        .expect("node");
        txn.save().expect("save");
    }
    // Declaring an additional, conforming type leaves `size` alone.
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&label(
        "id",
        BTreeMap::from([
            ("size".to_owned(), PropertyType::Int),
            ("id".to_owned(), PropertyType::String),
        ]),
    ))
    .expect("stage schema");
    txn.save()
        .expect("an unrelated, satisfied declaration must succeed");
}
