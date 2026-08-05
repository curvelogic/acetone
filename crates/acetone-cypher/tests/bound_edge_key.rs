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
/// `run`. NOTE the fixture deliberately differs from `import --disc` in
/// one respect: import puts the discriminator value ONLY in the key and
/// leaves the record empty, which makes the edges indistinguishable to a
/// `WHERE` until z093.5 re-exposes the discriminator as a readable
/// property — so this fixture ALSO stores `run` in the record, purely so
/// the tests can select one parallel edge. The genuinely-imported shape
/// is covered by the DETACH DELETE and delete-all tests below, which need
/// no selection.
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

/// The `deleted_edge_keys` site is load-bearing (PR #251 review F2): with
/// the pre-fix Null recompute, a DELETE of a discriminated edge put a
/// SPURIOUS Null key into the freed set, which let a CREATE in the same
/// statement silently overwrite a real, unrelated Null-keyed edge's
/// record while the edge told to die survived. Post-fix the create is
/// correctly refused as a duplicate.
#[test]
fn delete_plus_create_cannot_clobber_an_unrelated_null_edge() {
    let (_d, repo) = repo_with_parallel_edges();
    // Add a genuine Null-discriminated edge alongside the parallel pair.
    let mut txn = repo.begin_write().expect("begin");
    let a = NodeKey::new("Doc", vec![MV::Int(1)]).expect("key");
    let b = NodeKey::new("Doc", vec![MV::Int(2)]).expect("key");
    let plain = EdgeKey::new(a, "CITES".to_string(), b, MV::Null).expect("edge key");
    txn.put_edge(
        &plain,
        &EdgeRecord::new(BTreeMap::from([(
            "via".to_owned(),
            MV::String("original".into()),
        )])),
    )
    .expect("edge");
    txn.save().expect("save");
    assert_eq!(count_edges(&repo), 3);

    let err = Session::new(&repo)
        .run(
            "MATCH (a:Doc {id: 1})-[r:CITES]->(b:Doc {id: 2}) \
             WHERE r.run = 'r1' \
             DELETE r CREATE (a)-[:CITES {via: 'new'}]->(b)",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("conflicts with an existing relationship"),
        "the Null slot is occupied, so the create must refuse: {err}"
    );
    // Nothing landed: still three edges, the original record intact.
    assert_eq!(count_edges(&repo), 3);
    let outcome = Session::new(&repo)
        .run("MATCH ()-[r:CITES]->() WHERE r.via = 'original' RETURN count(r)")
        .expect("original intact");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    assert!(matches!(result.rows[0][0], RtValue::Int(1)));
}

/// DETACH DELETE cascades through `deleted_rels` and inherits the fix —
/// pre-fix it hard-errored with a dangling-relationship refusal because
/// the cascade "deleted" keys that did not exist (PR #251 review F2).
#[test]
fn detach_delete_removes_a_node_with_discriminated_edges() {
    let (_d, repo) = repo_with_parallel_edges();
    Session::new(&repo)
        .run("MATCH (a:Doc {id: 1}) DETACH DELETE a")
        .expect("detach delete must succeed over discriminated edges");
    assert_eq!(count_edges(&repo), 0, "the cascade must remove both edges");
    let outcome = Session::new(&repo)
        .run("MATCH (n:Doc) RETURN count(n)")
        .expect("count nodes");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    assert!(
        matches!(result.rows[0][0], RtValue::Int(1)),
        "only Doc 2 remains"
    );
}
