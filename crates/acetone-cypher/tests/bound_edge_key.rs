//! Bound-key fidelity for discriminated edges (`acetone-z093.4`, the
//! o8r hazard): `SET`/`DELETE` on a MATCHED edge must target exactly the
//! edge the query bound — including its discriminator — never a key
//! recomputed with `Null`.

use std::collections::BTreeMap;

use acetone_cypher::exec::value::Value as RtValue;
use acetone_cypher::session::{Outcome, Session};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value as MV;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, RelTypeDef, SchemaEntry};

/// Two `Doc` nodes joined by TWO parallel `CITES` edges discriminated by
/// `run` — written through the graph layer exactly as `import --disc`
/// writes them.
fn repo_with_parallel_edges() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Doc".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("schema");
    txn.put_schema(&SchemaEntry::RelType {
        name: "CITES".into(),
        def: RelTypeDef::new(Some("run".into()), Default::default(), []).expect("rel"),
    })
    .expect("rel schema");
    let a = NodeKey::new("Doc", vec![MV::Int(1)]).expect("key");
    let b = NodeKey::new("Doc", vec![MV::Int(2)]).expect("key");
    for key in [&a, &b] {
        txn.put_node(key, &NodeRecord::new([], BTreeMap::new()))
            .expect("node");
    }
    for run in ["r1", "r2"] {
        let edge = EdgeKey::new(
            a.clone(),
            "CITES".to_string(),
            b.clone(),
            MV::String(run.into()),
        )
        .expect("edge key");
        let record = EdgeRecord::new(BTreeMap::from([("run".to_owned(), MV::String(run.into()))]));
        txn.put_edge(&edge, &record).expect("edge");
    }
    txn.save().expect("save");
    (dir, repo)
}

fn count_edges(repo: &Repository) -> i64 {
    let outcome = Session::new(repo)
        .run("MATCH ()-[r:CITES]->() RETURN count(r)")
        .expect("count");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    match result.rows[0][0] {
        RtValue::Int(n) => n,
        ref other => panic!("int expected: {other:?}"),
    }
}

#[test]
fn delete_of_one_parallel_edge_removes_exactly_that_edge() {
    let (_d, repo) = repo_with_parallel_edges();
    assert_eq!(count_edges(&repo), 2, "fixture: two parallel edges");
    Session::new(&repo)
        .run("MATCH ()-[r:CITES]->() WHERE r.run = 'r1' DELETE r")
        .expect("delete one");
    assert_eq!(
        count_edges(&repo),
        1,
        "exactly the bound edge must be deleted"
    );
    // And the survivor is r2.
    let outcome = Session::new(&repo)
        .run("MATCH ()-[r:CITES]->() RETURN r.run")
        .expect("survivor");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    assert!(
        matches!(&result.rows[0][0], RtValue::String(s) if s == "r2"),
        "the r2 edge must survive: {:?}",
        result.rows[0][0]
    );
}

#[test]
fn set_on_one_parallel_edge_updates_it_in_place_without_a_phantom() {
    let (_d, repo) = repo_with_parallel_edges();
    Session::new(&repo)
        .run("MATCH ()-[r:CITES]->() WHERE r.run = 'r1' SET r.checked = true")
        .expect("set one");
    assert_eq!(
        count_edges(&repo),
        2,
        "SET must not mint a phantom third edge at the Null key"
    );
    let outcome = Session::new(&repo)
        .run("MATCH ()-[r:CITES]->() WHERE r.run = 'r1' RETURN r.checked")
        .expect("read back");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    assert!(
        matches!(result.rows[0][0], RtValue::Bool(true)),
        "the bound edge must carry the update: {:?}",
        result.rows[0][0]
    );
    // The sibling is untouched.
    let outcome = Session::new(&repo)
        .run("MATCH ()-[r:CITES]->() WHERE r.run = 'r2' RETURN r.checked")
        .expect("sibling");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    assert!(
        matches!(result.rows[0][0], RtValue::Null),
        "the sibling must be untouched: {:?}",
        result.rows[0][0]
    );
}

#[test]
fn undiscriminated_edges_are_unaffected() {
    // The common case (Null discriminator throughout) must behave
    // exactly as before the fix.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Doc".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("schema");
    txn.put_schema(&SchemaEntry::RelType {
        name: "CITES".into(),
        def: RelTypeDef::new(None, Default::default(), []).expect("rel"),
    })
    .expect("rel schema");
    txn.save().expect("save");
    let session = Session::new(&repo);
    session
        .run("CREATE (:Doc {id: 1})-[:CITES]->(:Doc {id: 2})")
        .expect("create");
    session
        .run("MATCH ()-[r:CITES]->() SET r.checked = true")
        .expect("set");
    session
        .run("MATCH ()-[r:CITES]->() DELETE r")
        .expect("delete");
    let outcome = session
        .run("MATCH ()-[r:CITES]->() RETURN count(r)")
        .expect("count");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    assert!(matches!(result.rows[0][0], RtValue::Int(0)));
}
