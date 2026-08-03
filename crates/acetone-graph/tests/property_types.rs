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
///
/// What gives this teeth is a **pre-existing** breach, which a single branch
/// can no longer create — so it is reached the way one genuinely remains
/// reachable: a conflicted merge, from three ordinary commits. One branch
/// declares `size: int` over data that conforms (there is none yet); the
/// other adds a string `size` while its own schema is still untyped. The
/// merged workspace carries both.
///
/// Without the `changed` filter, that breach would block an unrelated `id`
/// declaration — so a repository in this state, or one written before the
/// check existed, could not be maintained at all, only abandoned.
#[test]
fn an_unrelated_schema_change_does_not_re_judge_a_pre_existing_breach() {
    let (_dir, repo) = repo();
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&label("id", BTreeMap::new()))
        .expect("schema");
    let base = txn.commit("untyped", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&label(
        "id",
        BTreeMap::from([("size".to_owned(), PropertyType::Int)]),
    ))
    .expect("schema");
    txn.commit("declare size: int", &[], None).expect("commit");

    repo.checkout_branch("main").expect("checkout");
    let mut txn = repo.begin_write().expect("begin");
    txn.put_node(
        &NodeKey::new("Blob", vec![Value::String("a".into())]).expect("key"),
        &NodeRecord::new(
            [],
            BTreeMap::from([("size".to_owned(), Value::String("8".into()))]),
        ),
    )
    .expect("node");
    txn.commit("string size", &[], None).expect("commit");
    repo.merge("other", "merge").expect("merge");

    // Declaring `id: string` is satisfied, and unrelated to that breach.
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
        .expect("a satisfied declaration must not be blocked by an unrelated pre-existing breach");
}

// ─── Relationship property types (acetone-7qw.12) ───────────────────────────
// The edge counterparts of the checks above: declarable-but-inert until this
// unit, so each test pins one of the three enforcement points.

fn rel_type(types: BTreeMap<String, PropertyType>) -> SchemaEntry {
    SchemaEntry::RelType {
        name: "LINKS".into(),
        def: acetone_model::schema::RelTypeDef::new(None, types, []).expect("rel type"),
    }
}

fn seeded_edge_repo() -> (tempfile::TempDir, Repository) {
    let (dir, repo) = repo();
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&label("id", BTreeMap::new()))
        .expect("label");
    txn.put_schema(&rel_type(BTreeMap::new()))
        .expect("rel type");
    for id in ["a", "b"] {
        txn.put_node(
            &NodeKey::new("Blob", vec![Value::String(id.into())]).expect("key"),
            &NodeRecord::new([], BTreeMap::new()),
        )
        .expect("node");
    }
    txn.save().expect("seed");
    (dir, repo)
}

fn edge_key() -> acetone_model::graph_keys::EdgeKey {
    acetone_model::graph_keys::EdgeKey::new(
        NodeKey::new("Blob", vec![Value::String("a".into())]).expect("key"),
        "LINKS",
        NodeKey::new("Blob", vec![Value::String("b".into())]).expect("key"),
        Value::Null,
    )
    .expect("edge key")
}

/// Write-time chokepoint: a staged edge record contradicting the declared
/// relationship property type is refused at save, whatever wrote it.
#[test]
fn a_wrongly_typed_edge_property_is_refused_at_save() {
    let (_dir, repo) = seeded_edge_repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&rel_type(BTreeMap::from([(
            "weight".to_owned(),
            PropertyType::Int,
        )])))
        .expect("retype");
        txn.save().expect("no edges yet: retype lands clean");
    }
    let mut txn = repo.begin_write().expect("begin");
    txn.put_edge(
        &edge_key(),
        &acetone_model::records::EdgeRecord::new(BTreeMap::from([(
            "weight".to_owned(),
            Value::String("heavy".into()),
        )])),
    )
    .expect("stage edge");
    let err = txn
        .save()
        .expect_err("a string where int is declared must be refused");
    assert!(
        matches!(err, GraphError::PropertyTypeViolation { ref property, declared, actual, .. }
            if property == "weight" && declared == "int" && actual == "string"),
        "expected the edge chokepoint to refuse, got: {err}"
    );
}

/// Declare-time backfill: retyping a relationship property around existing
/// edges is refused — and the retype-plus-rewrite-in-one-transaction path
/// still lands, exactly as for labels.
#[test]
fn retyping_a_rel_property_around_existing_edges_is_refused() {
    let (_dir, repo) = seeded_edge_repo();
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_edge(
            &edge_key(),
            &acetone_model::records::EdgeRecord::new(BTreeMap::from([(
                "weight".to_owned(),
                Value::String("heavy".into()),
            )])),
        )
        .expect("edge under no declaration");
        txn.save().expect("save");
    }
    // Schema-only transaction: must be refused by the edge backfill.
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&rel_type(BTreeMap::from([(
        "weight".to_owned(),
        PropertyType::Int,
    )])))
    .expect("stage retype");
    let err = txn
        .save()
        .expect_err("declaring weight:int over a string-valued edge must refuse");
    assert!(
        matches!(err, GraphError::PropertyTypeViolation { ref property, .. } if property == "weight"),
        "expected the edge backfill to refuse, got: {err}"
    );
    // Retype AND repair in one transaction: allowed (the touched-edge skip).
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&rel_type(BTreeMap::from([(
        "weight".to_owned(),
        PropertyType::Int,
    )])))
    .expect("stage retype");
    txn.put_edge(
        &edge_key(),
        &acetone_model::records::EdgeRecord::new(BTreeMap::from([(
            "weight".to_owned(),
            Value::Int(9),
        )])),
    )
    .expect("stage repair");
    txn.save()
        .expect("a retype and its backfill may land in one transaction");
}
