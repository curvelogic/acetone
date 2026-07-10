//! Referential integrity enforced at the transaction boundary (ADR-0028,
//! Invariant #3, pre-0.1 review U5/U6): a `save`/`commit` must never leave an
//! edge without both its endpoint nodes. Two ways a transaction could break it
//! — putting an edge whose endpoint is absent, or deleting a node an edge still
//! references — are rejected before the workspace advances.

use acetone_graph::GraphError;
use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository, ResolveSide};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use std::collections::BTreeMap;
use std::path::Path;

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn edge(s: u8, d: u8) -> EdgeKey {
    EdgeKey::new(node(s), "R", node(d), Value::Null).expect("edge")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_string(), Value::Int(v))]))
}

/// Commit nodes 1 and 2 with an edge 1 -> 2 between them.
fn commit_two_nodes_and_edge(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(1)).expect("put 1");
    tx.put_node(&node(2), &record(2)).expect("put 2");
    tx.put_edge(&edge(1, 2), &EdgeRecord::default())
        .expect("edge");
    tx.commit("base", &[], None).expect("commit");
}

#[test]
fn putting_an_edge_to_a_missing_node_is_rejected() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(1)).expect("put 1");
    // node 2 is never created
    tx.put_edge(&edge(1, 2), &EdgeRecord::default())
        .expect("stage");
    match tx.save() {
        Err(GraphError::DanglingEdge { role, .. }) => assert_eq!(role, "target"),
        other => panic!("expected DanglingEdge (target), got {other:?}"),
    }
    // The workspace did not advance: no node, no edge persisted.
    let snap = repo.workspace_snapshot().expect("snap");
    assert!(snap.edges().expect("edges").is_empty());
    assert!(snap.get_node(&node(1)).expect("get").is_none());
}

#[test]
fn deleting_a_node_with_an_incident_edge_target_is_rejected() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init(dir.path());
    commit_two_nodes_and_edge(&repo);

    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(2)).expect("stage delete");
    match tx.save() {
        Err(GraphError::DanglingEdge { role, .. }) => assert_eq!(role, "target"),
        other => panic!("expected DanglingEdge (target), got {other:?}"),
    }
    // The node and edge survive: the rejected save did not advance the workspace.
    let snap = repo.workspace_snapshot().expect("snap");
    assert!(snap.get_node(&node(2)).expect("get").is_some());
    assert_eq!(snap.edges().expect("edges").len(), 1);
}

#[test]
fn deleting_a_node_that_is_an_edge_source_is_rejected() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init(dir.path());
    commit_two_nodes_and_edge(&repo);

    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(1)).expect("stage delete");
    match tx.save() {
        Err(GraphError::DanglingEdge { role, .. }) => assert_eq!(role, "source"),
        other => panic!("expected DanglingEdge (source), got {other:?}"),
    }
}

#[test]
fn deleting_a_node_and_its_incident_edge_together_succeeds() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init(dir.path());
    commit_two_nodes_and_edge(&repo);

    // Removing the edge in the same transaction leaves nothing dangling.
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_edge(&edge(1, 2)).expect("del edge");
    tx.delete_node(&node(2)).expect("del node");
    tx.save().expect("valid save must succeed");

    let snap = repo.workspace_snapshot().expect("snap");
    assert!(snap.edges().expect("edges").is_empty());
    assert!(snap.get_node(&node(2)).expect("get").is_none());
}

#[test]
fn completing_a_merge_whose_resolution_dangles_an_edge_is_rejected() {
    // U5 (pre-0.1 review): the transaction boundary already forbids deleting a
    // node under a live edge, so a dangling merge can only arise by *resolving*
    // an edge cell-conflict back over an endpoint the other side removed. Base
    // has node1, node2 and edge 1 -> 2. `theirs` removes node2 *and* its edge
    // (a valid deletion). `ours` modifies the edge's record. The merge deletes
    // node2 and leaves the edge as a cell conflict; resolving it to `ours`
    // restores edge 1 -> 2 over the now-absent node2. Completing that merge must
    // be rejected — never a silently-committed invalid graph. (If the merge
    // instead refuses up front as a graph violation, that is equally safe.)
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init(dir.path());

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(1)).expect("put 1");
    tx.put_node(&node(2), &record(2)).expect("put 2");
    tx.put_edge(
        &edge(1, 2),
        &EdgeRecord::new(BTreeMap::from([("w".into(), Value::Int(0))])),
    )
    .expect("edge");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_edge(&edge(1, 2)).expect("delete edge");
    tx.delete_node(&node(2)).expect("delete node");
    tx.commit("theirs removes node2 and its edge", &[], None)
        .expect("commit");

    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(
        &edge(1, 2),
        &EdgeRecord::new(BTreeMap::from([("w".into(), Value::Int(9))])),
    )
    .expect("modify edge");
    tx.commit("ours modifies the edge", &[], None)
        .expect("commit");

    let outcome = repo.merge("other", "merge other").expect("merge");
    assert!(
        matches!(outcome, MergeOutcome::Conflicts(_)),
        "expected conflicts, got {outcome:?}"
    );
    // This scenario deterministically enters merge-in-progress (an edge *cell*
    // conflict), so the transaction-boundary backstop — not an up-front graph
    // refusal — is what must catch the dangling resolution. Lock that coverage
    // in: were a future merge refactor to refuse up front instead, this assert
    // fails loudly rather than letting the test pass without exercising the
    // backstop.
    assert!(
        repo.merge_head().expect("merge head").is_some(),
        "expected merge-in-progress so the resolve→save backstop is exercised"
    );
    // Resolving the edge cell-conflict to `ours` restores edge 1 -> 2 over the
    // now-absent node2; it must be rejected as a dangling edge — at the resolving
    // save or, failing that, at commit. Either way the merge cannot complete
    // into an invalid graph.
    match repo.resolve_all(ResolveSide::Ours) {
        Err(GraphError::DanglingEdge { .. }) => {}
        Ok(_) => {
            let tx = repo.begin_write().expect("begin");
            match tx.commit("complete", &[], None) {
                Err(GraphError::DanglingEdge { .. }) => {}
                other => panic!("completing a dangling merge must be rejected, got {other:?}"),
            }
        }
        Err(other) => panic!("unexpected resolve error: {other:?}"),
    }
}
