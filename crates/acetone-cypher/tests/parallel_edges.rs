//! Cypher-created parallel edges (`acetone-z093.5`): a declared
//! discriminator resolves from the property map into the edge key at
//! CREATE/MERGE, is re-exposed on read under its declared name, and is
//! immutable under SET.

use std::collections::BTreeMap;

use acetone_cypher::exec::value::Value as RtValue;
use acetone_cypher::session::{Outcome, Session};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value as MV;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, RelTypeDef, SchemaEntry};

fn repo_with_disc_type() -> (tempfile::TempDir, Repository) {
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
    for id in [1, 2] {
        txn.put_node(
            &NodeKey::new("Doc", vec![MV::Int(id)]).expect("key"),
            &NodeRecord::new([], BTreeMap::new()),
        )
        .expect("node");
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
fn create_makes_parallel_edges_per_discriminator_value() {
    let (_d, repo) = repo_with_disc_type();
    let session = Session::new(&repo);
    for run in ["r1", "r2"] {
        session
            .run(&format!(
                "MATCH (a:Doc {{id: 1}}), (b:Doc {{id: 2}}) \
                 CREATE (a)-[:CITES {{run: '{run}', note: 'n-{run}'}}]->(b)"
            ))
            .expect("create parallel edge");
    }
    assert_eq!(count_edges(&repo), 2, "two parallel edges must coexist");
    // Each is matchable by its re-exposed discriminator, carrying its own
    // record properties.
    for run in ["r1", "r2"] {
        let outcome = session
            .run(&format!(
                "MATCH ()-[r:CITES]->() WHERE r.run = '{run}' RETURN r.note"
            ))
            .expect("select one");
        let Outcome::Read(result) = outcome else {
            panic!("read expected");
        };
        assert_eq!(result.rows.len(), 1, "{run}");
        assert!(
            matches!(&result.rows[0][0], RtValue::String(s) if s == &format!("n-{run}")),
            "{run}: {:?}",
            result.rows[0][0]
        );
    }
}

#[test]
fn same_discriminator_value_is_still_a_duplicate() {
    let (_d, repo) = repo_with_disc_type();
    let session = Session::new(&repo);
    session
        .run(
            "MATCH (a:Doc {id: 1}), (b:Doc {id: 2}) \
             CREATE (a)-[:CITES {run: 'r1'}]->(b)",
        )
        .expect("first");
    let err = session
        .run(
            "MATCH (a:Doc {id: 1}), (b:Doc {id: 2}) \
             CREATE (a)-[:CITES {run: 'r1'}]->(b)",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("conflicts with an existing relationship"),
        "same identity must refuse: {err}"
    );
    assert_eq!(count_edges(&repo), 1);
}

#[test]
fn a_missing_declared_discriminator_is_refused() {
    let (_d, repo) = repo_with_disc_type();
    let err = Session::new(&repo)
        .run(
            "MATCH (a:Doc {id: 1}), (b:Doc {id: 2}) \
             CREATE (a)-[:CITES {note: 'no run'}]->(b)",
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("discriminator") && msg.contains("run"),
        "the refusal must name the property: {msg}"
    );
    assert_eq!(count_edges(&repo), 0, "nothing may land");
}

#[test]
fn import_shape_edges_expose_the_discriminator_on_read() {
    // The genuinely-imported shape: value in the KEY only, record empty
    // (the PR #251 F3 gap, closed by read-side re-exposure).
    let (_d, repo) = repo_with_disc_type();
    let mut txn = repo.begin_write().expect("begin");
    let a = NodeKey::new("Doc", vec![MV::Int(1)]).expect("key");
    let b = NodeKey::new("Doc", vec![MV::Int(2)]).expect("key");
    let edge = EdgeKey::new(a, "CITES".to_string(), b, MV::String("r9".into())).expect("edge");
    txn.put_edge(&edge, &EdgeRecord::new(BTreeMap::new()))
        .expect("edge");
    txn.save().expect("save");

    let outcome = Session::new(&repo)
        .run("MATCH ()-[r:CITES]->() WHERE r.run = 'r9' RETURN r.run")
        .expect("select by re-exposed disc");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    assert_eq!(result.rows.len(), 1, "record-empty edge must be selectable");
    assert!(matches!(&result.rows[0][0], RtValue::String(s) if s == "r9"));
}

#[test]
fn set_of_the_discriminator_is_refused_and_unchanged_value_is_fine() {
    let (_d, repo) = repo_with_disc_type();
    let session = Session::new(&repo);
    session
        .run(
            "MATCH (a:Doc {id: 1}), (b:Doc {id: 2}) \
             CREATE (a)-[:CITES {run: 'r1'}]->(b)",
        )
        .expect("create");
    let err = session
        .run("MATCH ()-[r:CITES]->() SET r.run = 'r2'")
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot modify discriminator"),
        "identity is immutable: {err}"
    );
    // Same value: allowed, no-op on the key, other properties land.
    session
        .run("MATCH ()-[r:CITES]->() SET r.run = 'r1', r.checked = true")
        .expect("no-op disc set");
    assert_eq!(count_edges(&repo), 1, "no phantom");
    let outcome = session
        .run("MATCH ()-[r:CITES]->() WHERE r.run = 'r1' RETURN r.checked")
        .expect("read back");
    let Outcome::Read(result) = outcome else {
        panic!("read expected");
    };
    assert!(matches!(result.rows[0][0], RtValue::Bool(true)));
}

#[test]
fn merge_matches_per_discriminator_value() {
    let (_d, repo) = repo_with_disc_type();
    let session = Session::new(&repo);
    for _ in 0..2 {
        session
            .run(
                "MATCH (a:Doc {id: 1}), (b:Doc {id: 2}) \
                 MERGE (a)-[:CITES {run: 'r1'}]->(b)",
            )
            .expect("merge r1");
    }
    session
        .run(
            "MATCH (a:Doc {id: 1}), (b:Doc {id: 2}) \
             MERGE (a)-[:CITES {run: 'r2'}]->(b)",
        )
        .expect("merge r2");
    assert_eq!(
        count_edges(&repo),
        2,
        "MERGE is idempotent per value and creates per new value"
    );
}
