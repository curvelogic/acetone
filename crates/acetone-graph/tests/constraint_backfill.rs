//! The declare-time backfill check and the plumbing-write guard must keep
//! `check_nodes`' exact semantics — same violations, same deterministic
//! order — while streaming the label instead of materialising the graph
//! (acetone-2ck.20). These tests pin that equivalence differentially: the
//! expected value is computed by materialising a `NodeSet` and calling
//! `check_nodes` directly, the way both functions used to.

use std::collections::{BTreeMap, BTreeSet};

use acetone_graph::constraints::{NodeSet, check_label, check_nodes, check_upsert};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{LabelDef, PropertyType, SchemaEntry};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    (dir, repo)
}

fn node_key(label: &str, name: &str) -> NodeKey {
    NodeKey::new(label, vec![Value::String(name.into())]).expect("key")
}

fn record(props: &[(&str, Value)]) -> NodeRecord {
    NodeRecord::new(
        std::iter::empty::<String>(),
        props
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

/// A definition exercising all three violation kinds at once: `tier` is
/// required, `port` is declared `int`, and `serial` is UNIQUE.
fn service_def() -> LabelDef {
    LabelDef::new(
        vec!["name".to_owned()],
        BTreeMap::from([("port".to_owned(), PropertyType::Int)]),
        ["tier".to_owned()],
        ["serial".to_owned()],
    )
    .expect("def")
}

/// Nodes of the target label plus noise labels sorting both before and
/// after it in prefix order, so a scan that leaks across the label
/// boundary shows up as a differential mismatch.
fn seed(repo: &Repository) -> Vec<(NodeKey, NodeRecord)> {
    let nodes = vec![
        // Sorts before "Service"; violates the Service def on every axis,
        // which must not matter — it is not a Service.
        (
            node_key("Aardvark", "x"),
            record(&[("port", Value::String("p".into()))]),
        ),
        // Conforming.
        (
            node_key("Service", "a"),
            record(&[
                ("tier", Value::String("gold".into())),
                ("port", Value::Int(80)),
                ("serial", Value::String("s-1".into())),
            ]),
        ),
        // Missing `tier`, wrong-typed `port`, and half of a `serial` collision.
        (
            node_key("Service", "b"),
            record(&[
                ("port", Value::String("eighty".into())),
                ("serial", Value::String("s-2".into())),
            ]),
        ),
        // The other half of the collision, plus its own missing `tier`.
        (
            node_key("Service", "c"),
            record(&[("serial", Value::String("s-2".into()))]),
        ),
        // A second, distinct collision group of three.
        (
            node_key("Service", "d"),
            record(&[
                ("tier", Value::String("t".into())),
                ("serial", Value::Int(7)),
            ]),
        ),
        (
            node_key("Service", "e"),
            record(&[
                ("tier", Value::String("t".into())),
                ("serial", Value::Int(7)),
            ]),
        ),
        (
            node_key("Service", "f"),
            record(&[
                ("tier", Value::String("t".into())),
                ("serial", Value::Int(7)),
            ]),
        ),
        // Sorts after "Service".
        (node_key("Zebra", "z"), record(&[])),
    ];
    let mut txn = repo.begin_write().expect("begin");
    for (key, rec) in &nodes {
        txn.put_node(key, rec).expect("put");
    }
    txn.save().expect("save");
    nodes
}

fn service_set(nodes: &[(NodeKey, NodeRecord)]) -> NodeSet {
    nodes
        .iter()
        .filter(|(k, _)| k.label() == "Service")
        .map(|(k, r)| (k.encode().expect("enc"), (k.clone(), r.clone())))
        .collect()
}

#[test]
fn check_label_matches_check_nodes_exactly() {
    let (_dir, repo) = repo();
    let nodes = seed(&repo);
    let def = service_def();

    let expected = check_nodes(
        &BTreeMap::from([("Service".to_owned(), def.clone())]),
        &service_set(&nodes),
        None,
    )
    .expect("reference");
    // The fixture exercises every violation kind, or it proves nothing.
    assert!(expected.len() >= 5, "fixture too weak: {expected:?}");

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let streamed = check_label(&snapshot, "Service", &def).expect("streamed");
    assert_eq!(streamed, expected);
}

#[test]
fn check_label_on_a_clean_label_is_empty() {
    let (_dir, repo) = repo();
    seed(&repo);
    // Zebra has no properties and the def constrains none it lacks as key.
    let def = LabelDef::new(vec!["name".to_owned()], BTreeMap::new(), [], []).expect("def");
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    assert!(
        check_label(&snapshot, "Zebra", &def)
            .expect("check")
            .is_empty()
    );
}

/// `check_upsert` judges the workspace as it would be *after* the put:
/// focus-only reporting, no self-collision on replace, collisions listed
/// with the focus node in key order. Each case is asserted against the
/// materialise-and-`check_nodes` reference the function used to be.
#[test]
fn check_upsert_focus_semantics() {
    let (_dir, repo) = repo();
    let def = service_def();
    // Declare first, then write conforming data, then probe upserts.
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_schema(&SchemaEntry::Label {
            name: "Service".to_owned(),
            def: def.clone(),
        })
        .expect("declare");
        txn.save().expect("save");
    }
    let existing = vec![
        (
            node_key("Service", "a"),
            record(&[
                ("tier", Value::String("gold".into())),
                ("serial", Value::String("s-1".into())),
            ]),
        ),
        (
            node_key("Service", "m"),
            record(&[
                ("tier", Value::String("gold".into())),
                ("serial", Value::String("s-9".into())),
            ]),
        ),
    ];
    {
        let mut txn = repo.begin_write().expect("begin");
        for (key, rec) in &existing {
            txn.put_node(key, rec).expect("put");
        }
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");

    let reference = |key: &NodeKey, rec: &NodeRecord| {
        let mut set: NodeSet = existing
            .iter()
            .map(|(k, r)| (k.encode().expect("enc"), (k.clone(), r.clone())))
            .collect();
        let encoded = key.encode().expect("enc");
        let focus: BTreeSet<Vec<u8>> = [encoded.clone()].into_iter().collect();
        set.insert(encoded, (key.clone(), rec.clone()));
        check_nodes(
            &BTreeMap::from([("Service".to_owned(), def.clone())]),
            &set,
            Some(&focus),
        )
        .expect("reference")
    };

    // Replacing `a` with itself, same serial: never a self-collision.
    let (key_a, rec_a) = &existing[0];
    let same = check_upsert(&snapshot, key_a, rec_a).expect("upsert");
    assert_eq!(same, reference(key_a, rec_a));
    assert!(same.is_empty(), "{same:?}");

    // A new node colliding with `m`'s serial, missing `tier`, wrong-typed
    // `port` — every violation kind, all focus-involving. The collision
    // members must come out in node-key order with the focus node placed
    // correctly both before (`b` < `m`) and after (`z` > `m`).
    for name in ["b", "z"] {
        let key = node_key("Service", name);
        let rec = record(&[
            ("port", Value::String("eighty".into())),
            ("serial", Value::String("s-9".into())),
        ]);
        let got = check_upsert(&snapshot, &key, &rec).expect("upsert");
        assert_eq!(got, reference(&key, &rec));
        assert_eq!(got.len(), 3, "{got:?}");
    }

    // A pre-existing breach elsewhere is not the focus write's business:
    // `a` keeps its serial; upserting an unrelated conforming node reports
    // nothing even though we sneak a duplicate of `s-1` in as `q` first.
    {
        let mut txn = repo.begin_write().expect("begin");
        txn.put_node(
            &node_key("Service", "q"),
            &record(&[
                ("tier", Value::String("t".into())),
                ("serial", Value::String("s-1".into())),
            ]),
        )
        .expect("put dup");
        // Plumbing writes stage without UNIQUE enforcement — that guard is
        // exactly what `check_upsert` exists to give the CLI — so the
        // collision lands and becomes the pre-existing breach.
        txn.save().expect("save");
    }
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    let clean_key = node_key("Service", "r");
    let clean_rec = record(&[
        ("tier", Value::String("t".into())),
        ("serial", Value::String("s-r".into())),
    ]);
    let got = check_upsert(&snapshot, &clean_key, &clean_rec).expect("upsert");
    assert!(got.is_empty(), "{got:?}");
}
