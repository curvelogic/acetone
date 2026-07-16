//! The merge-in-progress state machine (spec §6, acetone-14c.4a): a
//! conflicted merge persists its conflicts and enters merge-in-progress
//! state; `resolve_all` picks a side; `commit` completes the merge as a
//! two-parent commit and clears the state.

use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository, ResolveSide};
use acetone_graph::{GraphError, fsck};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_store::{CommitStore, Hash};
use std::collections::BTreeMap;
use std::path::Path;

fn edge(s: u8, d: u8) -> EdgeKey {
    EdgeKey::new(node(s), "R", node(d), Value::Null).expect("edge")
}

fn edge_record(w: i64) -> EdgeRecord {
    EdgeRecord::new(BTreeMap::from([("w".to_string(), Value::Int(w))]))
}

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_string(), Value::Int(v))]))
}

fn commit_node(repo: &Repository, id: u8, v: i64, message: &str) -> Hash {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(id), &record(v)).expect("put");
    tx.commit(message, &[], None).expect("commit")
}

/// The `v` property of node `id` in the current workspace, or `None` when the
/// node is absent *or* its `v` is (a conflicted-away property under cell-wise
/// merge, ADR-0035).
fn workspace_v(repo: &Repository, id: u8) -> Option<i64> {
    let snap = repo.workspace_snapshot().expect("snapshot");
    snap.get_node(&node(id))
        .expect("get")
        .and_then(|r| match r.properties().get("v") {
            Some(Value::Int(n)) => Some(*n),
            None => None,
            other => panic!("unexpected v: {other:?}"),
        })
}

/// Set up base=1:10, ours(main)=1:11, theirs(other)=1:12 and merge — a single
/// cell conflict on node 1. Returns (ours_commit, theirs_commit).
fn conflicting_merge(repo: &Repository) -> (Hash, Hash) {
    let base = commit_node(repo, 1, 10, "base");
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let theirs = commit_node(repo, 1, 12, "other sets 1=12");
    repo.checkout_branch("main").expect("checkout");
    let ours = commit_node(repo, 1, 11, "main sets 1=11");
    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(c) => assert_eq!(c.len(), 1),
        other => panic!("expected Conflicts, got {other:?}"),
    }
    (ours, theirs)
}

#[test]
fn a_conflicted_merge_enters_merge_in_progress_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (ours, theirs) = conflicting_merge(&repo);

    // The branch has not moved, but the merge is in progress.
    assert_eq!(repo.head_commit().expect("head"), Some(ours));
    assert_eq!(repo.merge_head().expect("merge head"), Some(theirs));
    assert_eq!(repo.conflicts().expect("conflicts").len(), 1);
    // Under cell-wise merge (ADR-0035) the node itself is present — only its
    // single conflicted property `v` is withheld until the conflict resolves.
    let snap = repo.workspace_snapshot().expect("snapshot");
    assert!(
        snap.get_node(&node(1)).expect("get").is_some(),
        "the node stays in the graph; only the conflicted property is withheld"
    );
    assert_eq!(workspace_v(&repo, 1), None);
}

#[test]
fn commit_refuses_while_conflicts_remain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    conflicting_merge(&repo);

    let txn = repo.begin_write().expect("begin");
    let err = txn.commit("finish", &[], None).expect_err("must refuse");
    assert!(
        matches!(err, GraphError::MergeState(_)),
        "expected MergeState, got {err:?}"
    );
}

#[test]
fn resolve_all_ours_then_commit_completes_the_merge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (ours, theirs) = conflicting_merge(&repo);

    assert_eq!(repo.resolve_all(ResolveSide::Ours).expect("resolve"), 1);
    // Ours' value (11) is now in the graph; no conflicts remain.
    assert_eq!(workspace_v(&repo, 1), Some(11));
    assert!(repo.conflicts().expect("conflicts").is_empty());

    let txn = repo.begin_write().expect("begin");
    let merge_commit = txn.commit("merge other", &[], None).expect("commit");

    // The merge commit has both tips as parents, and the state is cleared.
    let commit = repo
        .store()
        .read_commit(&merge_commit)
        .expect("read")
        .unwrap();
    assert_eq!(commit.parents, vec![ours, theirs]);
    assert_eq!(repo.head_commit().expect("head"), Some(merge_commit));
    assert!(repo.merge_head().expect("merge head").is_none());
    assert!(!repo.is_dirty().expect("dirty"));
    assert!(!fsck(&repo).expect("fsck").has_errors());
}

#[test]
fn resolve_all_theirs_picks_the_other_side() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    conflicting_merge(&repo);

    assert_eq!(repo.resolve_all(ResolveSide::Theirs).expect("resolve"), 1);
    // Theirs' value (12) is chosen.
    assert_eq!(workspace_v(&repo, 1), Some(12));
    let txn = repo.begin_write().expect("begin");
    txn.commit("merge other", &[], None).expect("commit");
    assert_eq!(workspace_v(&repo, 1), Some(12));
    assert!(repo.merge_head().expect("merge head").is_none());
}

#[test]
fn resolves_an_edge_cell_conflict_and_keeps_edges_rev_symmetric() {
    // Both sides modify edge (1)-[R]->(2)'s record differently -> a cell
    // conflict on the edge. Resolving must maintain edges_rev (Invariant #5).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(0)).expect("put");
    tx.put_node(&node(2), &record(0)).expect("put");
    tx.put_edge(&edge(1, 2), &edge_record(0)).expect("edge");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &edge_record(2)).expect("edge");
    tx.commit("theirs edits edge", &[], None).expect("commit");

    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &edge_record(1)).expect("edge");
    tx.commit("ours edits edge", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(c) => assert_eq!(c.len(), 1),
        other => panic!("expected Conflicts, got {other:?}"),
    }
    assert_eq!(repo.resolve_all(ResolveSide::Theirs).expect("resolve"), 1);

    // Theirs' edge record (w=2) is chosen.
    let snap = repo.workspace_snapshot().expect("snapshot");
    let edges = snap.edges().expect("edges");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].1.properties().get("w"), Some(&Value::Int(2)));

    let txn = repo.begin_write().expect("begin");
    txn.commit("merge other", &[], None).expect("commit");
    // fsck checks forward/reverse edge-map symmetry.
    assert!(!fsck(&repo).expect("fsck").has_errors());
}

#[test]
fn resolve_ours_on_a_delete_vs_modify_conflict_deletes_the_node() {
    // ours deletes node 1; theirs modifies it -> a cell conflict. Resolving
    // to ours (which has the key absent) deletes it.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    commit_node(&repo, 1, 12, "theirs modifies 1");
    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(1)).expect("delete");
    tx.commit("ours deletes 1", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(c) => assert_eq!(c.len(), 1),
        other => panic!("expected Conflicts, got {other:?}"),
    }
    assert_eq!(repo.resolve_all(ResolveSide::Ours).expect("resolve"), 1);
    // Ours deleted it, so the node stays gone.
    assert_eq!(workspace_v(&repo, 1), None);
    let txn = repo.begin_write().expect("begin");
    txn.commit("merge other", &[], None).expect("commit");
    assert_eq!(workspace_v(&repo, 1), None);
    assert!(repo.merge_head().expect("merge head").is_none());
}

#[test]
fn an_ordinary_write_to_a_conflicted_key_resolves_it() {
    // Spec §6: conflicts resolve by ordinary writes too. Writing a custom
    // merged value for the conflicted node clears its conflict, letting the
    // merge complete with that value.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (ours, theirs) = conflicting_merge(&repo);

    // Resolve node 1 by writing a hand-merged value (neither ours nor theirs).
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(99)).expect("put");
    tx.save().expect("save");

    // The conflict is gone; the write's value stands.
    assert!(repo.conflicts().expect("conflicts").is_empty());
    assert_eq!(workspace_v(&repo, 1), Some(99));

    // Commit completes the merge as a two-parent commit.
    let txn = repo.begin_write().expect("begin");
    let merge_commit = txn
        .commit("merge (resolved by write)", &[], None)
        .expect("commit");
    let commit = repo
        .store()
        .read_commit(&merge_commit)
        .expect("read")
        .unwrap();
    assert_eq!(commit.parents, vec![ours, theirs]);
    assert!(repo.merge_head().expect("merge head").is_none());
    assert_eq!(workspace_v(&repo, 1), Some(99));
}

#[test]
fn an_edge_conflict_resolves_by_writing_the_edge() {
    // The riskiest by-write path (edges_rev symmetry): both sides modify an
    // edge; writing a merged edge record clears the conflict and keeps the
    // reverse map in sync.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(0)).expect("put");
    tx.put_node(&node(2), &record(0)).expect("put");
    tx.put_edge(&edge(1, 2), &edge_record(0)).expect("edge");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &edge_record(2)).expect("edge");
    tx.commit("theirs", &[], None).expect("commit");
    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &edge_record(1)).expect("edge");
    tx.commit("ours", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(c) => assert_eq!(c.len(), 1),
        other => panic!("expected Conflicts, got {other:?}"),
    }

    // Resolve by writing a hand-merged edge record.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &edge_record(9)).expect("edge");
    tx.save().expect("save");
    assert!(repo.conflicts().expect("conflicts").is_empty());

    let txn = repo.begin_write().expect("begin");
    txn.commit("merge", &[], None).expect("commit");
    let snap = repo.workspace_snapshot().expect("snapshot");
    assert_eq!(
        snap.edges().expect("edges")[0].1.properties().get("w"),
        Some(&Value::Int(9))
    );
    // Reverse map stayed symmetric.
    assert!(!fsck(&repo).expect("fsck").has_errors());
}

#[test]
fn a_write_to_an_unconflicted_key_leaves_conflicts_intact() {
    // Writing some other node during a merge does not spuriously clear the
    // conflict on node 1.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    conflicting_merge(&repo);

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(2), &record(20)).expect("put");
    tx.save().expect("save");

    // Node 1's conflict still stands; the merge is not yet completable.
    assert_eq!(repo.conflicts().expect("conflicts").len(), 1);
    let txn = repo.begin_write().expect("begin");
    assert!(matches!(
        txn.commit("nope", &[], None),
        Err(GraphError::MergeState(_))
    ));
}

#[test]
fn resolve_with_no_merge_in_progress_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    commit_node(&repo, 1, 10, "base");
    let err = repo
        .resolve_all(ResolveSide::Ours)
        .expect_err("no merge to resolve");
    assert!(matches!(err, GraphError::MergeState(_)), "got {err:?}");
}
