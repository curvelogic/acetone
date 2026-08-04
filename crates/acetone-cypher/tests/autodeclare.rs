//! Relationship-type autodeclare (ADR-0060, `acetone-nc91`): strictly
//! opt-in coinage of unknown relationship types in CREATE/MERGE position,
//! appended to the schema in the same transaction as the data. Off by
//! default; reads never coin.

use std::collections::BTreeMap;

use acetone_cypher::session::{Outcome, QueryError, Session};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value as MV;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{LabelDef, SchemaEntry};

fn repo() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    (dir, repo)
}

/// Two `Entity` nodes to hang coined relationships between.
fn seed(repo: &Repository) {
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Entity".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("schema");
    for id in [1, 2] {
        txn.put_node(
            &NodeKey::new("Entity", vec![MV::Int(id)]).expect("key"),
            &NodeRecord::new([], BTreeMap::new()),
        )
        .expect("node");
    }
    txn.save().expect("save");
}

const COIN: &str = "MATCH (a:Entity {id: 1}), (b:Entity {id: 2}) \
                    CREATE (a)-[:MENTORS]->(b)";

#[test]
fn off_by_default_an_unknown_type_still_errors() {
    let (_d, repo) = repo();
    seed(&repo);
    let err = Session::new(&repo).run(COIN).unwrap_err();
    assert!(
        matches!(err, QueryError::Bind(_)),
        "expected the UnknownRelType bind error, got: {err}"
    );
    // And the builder set to off is identical to the default.
    let err = Session::new(&repo)
        .autodeclare(false)
        .run(COIN)
        .unwrap_err();
    assert!(matches!(err, QueryError::Bind(_)));
}

#[test]
fn opt_in_coins_the_type_stores_the_edge_and_advises() {
    let (_d, repo) = repo();
    seed(&repo);
    let session = Session::new(&repo).autodeclare(true);
    let outcome = session.run(COIN).expect("coining write");
    let Outcome::Write(result) = outcome else {
        panic!("expected a write outcome");
    };
    assert!(
        result
            .advisories
            .iter()
            .any(|a| a.contains("autodeclared relationship type") && a.contains("MENTORS")),
        "advisory must announce the coinage: {:?}",
        result.advisories
    );
    // The edge is queryable — and by a session WITHOUT autodeclare, since
    // the type is now genuinely declared.
    let outcome = Session::new(&repo)
        .run("MATCH (:Entity {id: 1})-[r:MENTORS]->(b:Entity) RETURN b.id")
        .expect("read back");
    let Outcome::Read(result) = outcome else {
        panic!("expected a read outcome");
    };
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn reads_never_coin_even_with_the_flag_on() {
    let (_d, repo) = repo();
    seed(&repo);
    let err = Session::new(&repo)
        .autodeclare(true)
        .run("MATCH ()-[r:NEVER_SEEN]->() RETURN r")
        .unwrap_err();
    assert!(
        matches!(err, QueryError::Bind(_)),
        "a read-position unknown type must stay an error: {err}"
    );
}

#[test]
fn merge_position_coins_too() {
    let (_d, repo) = repo();
    seed(&repo);
    let session = Session::new(&repo).autodeclare(true);
    session
        .run(
            "MATCH (a:Entity {id: 1}), (b:Entity {id: 2}) \
             MERGE (a)-[:CITES]->(b)",
        )
        .expect("MERGE coining write");
    // MERGE again: now matches, no duplicate, type declared once.
    session
        .run(
            "MATCH (a:Entity {id: 1}), (b:Entity {id: 2}) \
             MERGE (a)-[:CITES]->(b)",
        )
        .expect("idempotent MERGE");
    let outcome = Session::new(&repo)
        .run("MATCH ()-[r:CITES]->() RETURN r")
        .expect("read back");
    let Outcome::Read(result) = outcome else {
        panic!("expected a read outcome");
    };
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn a_later_clause_may_read_the_type_this_query_coins() {
    let (_d, repo) = repo();
    seed(&repo);
    let outcome = Session::new(&repo)
        .autodeclare(true)
        .run(
            "MATCH (a:Entity {id: 1}), (b:Entity {id: 2}) \
             CREATE (a)-[:LINKS]->(b) \
             WITH a MATCH (a)-[r:LINKS]->(c) RETURN c.id",
        )
        .expect("coin then read in one query");
    let Outcome::Write(result) = outcome else {
        panic!("expected a write outcome");
    };
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn duplicate_coinage_in_one_query_declares_once() {
    let (_d, repo) = repo();
    seed(&repo);
    let outcome = Session::new(&repo)
        .autodeclare(true)
        .run(
            "MATCH (a:Entity {id: 1}), (b:Entity {id: 2}) \
             CREATE (a)-[:KNOWS]->(b), (b)-[:KNOWS]->(a)",
        )
        .expect("double coinage");
    let Outcome::Write(result) = outcome else {
        panic!("expected a write outcome");
    };
    let count = result
        .advisories
        .iter()
        .filter(|a| a.contains("autodeclared"))
        .count();
    assert_eq!(
        count, 1,
        "one advisory per coined type: {:?}",
        result.advisories
    );
}

/// Two branches coining the SAME type converge in merge without conflict —
/// the coined definition is deterministic (empty), so both sides write the
/// identical schema entry (ADR-0060's determinism requirement).
#[test]
fn convergent_coinage_across_branches_merges_cleanly() {
    let (_d, repo) = repo();
    seed(&repo);
    let base = repo
        .begin_write()
        .expect("begin")
        .commit("base", &[], None)
        .expect("commit")
        .to_hex();
    repo.create_branch("left", Some(&base))
        .expect("branch left");
    repo.create_branch("right", Some(&base))
        .expect("branch right");

    for branch in ["left", "right"] {
        repo.checkout_branch(branch).expect("checkout");
        Session::new(&repo)
            .autodeclare(true)
            .run(COIN)
            .expect("coin on branch");
        repo.begin_write()
            .expect("begin")
            .commit(&format!("coin on {branch}"), &[], None)
            .expect("commit");
    }

    repo.checkout_branch("left").expect("back to left");
    let merged = repo.merge("right", "merge right").expect("merge");
    assert!(
        matches!(merged, acetone_graph::merge::MergeOutcome::Merged(_)),
        "identical coined definitions must not conflict: {merged:?}"
    );
}
