//! Merge lifecycle: abort, graph-violation resolution by ordinary writes,
//! completion re-validation, and defensive MERGE_HEAD handling (acetone-mws,
//! folding in acetone-36y). Builds on the merge-in-progress state machine
//! (acetone-14c.4a) and the cell-wise conflict model (ADR-0035).

use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_graph::{GraphError, fsck};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, SchemaEntry};
use acetone_store::{CommitStore, RefStore};
use std::collections::BTreeMap;
use std::path::Path;

const MERGE_HEAD: &str = "refs/worktree/acetone/merge-head";

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn edge(s: u8, d: u8) -> EdgeKey {
    EdgeKey::new(node(s), "R", node(d), Value::Null).expect("edge")
}

fn record(props: &[(&str, Value)]) -> NodeRecord {
    NodeRecord::new(
        [],
        props
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

/// Base graph with nodes 1 and 2, forked into `other` (theirs) and `main`
/// (ours). Returns (ours_commit, theirs_commit) after running the edit
/// closures. A dangling-edge setup: theirs deletes node 2, ours adds edge 1→2.
fn dangling_merge_in_progress(repo: &Repository) -> (acetone_store::Hash, acetone_store::Hash) {
    let mut tx = repo.begin_write().expect("begin");
    for id in [1, 2] {
        tx.put_node(&node(id), &record(&[])).expect("put");
    }
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(2)).expect("delete");
    let theirs = tx.commit("theirs deletes 2", &[], None).expect("commit");

    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &EdgeRecord::default())
        .expect("edge");
    let ours = tx.commit("ours adds 1->2", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(_) => {}
        other => panic!("expected a graph-violation conflict, got {other:?}"),
    }
    assert!(repo.merge_head().expect("merge head").is_some());
    (ours, theirs)
}

#[test]
fn merge_abort_restores_the_branch_tip_and_clears_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (ours, _theirs) = dangling_merge_in_progress(&repo);

    repo.abort_merge().expect("abort");

    // No merge in progress; workspace is clean and back at ours.
    assert!(repo.merge_head().expect("merge head").is_none());
    assert!(!repo.is_dirty().expect("dirty"));
    assert_eq!(repo.head_commit().expect("head"), Some(ours));
    // ours' graph is intact: node 2 and the edge both present, fsck clean.
    let snap = repo.workspace_snapshot().expect("snapshot");
    assert!(snap.get_node(&node(2)).expect("get").is_some());
    assert!(!fsck::check(&repo).expect("fsck").has_errors());
}

#[test]
fn abort_with_no_merge_in_progress_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(&[])).expect("put");
    tx.commit("base", &[], None).expect("commit");
    let err = repo.abort_merge().expect_err("must error");
    assert!(matches!(err, GraphError::MergeState(_)), "got {err:?}");
}

#[test]
fn abort_recovers_a_half_aborted_state_with_no_merge_head() {
    // A prior abort that cleared MERGE_HEAD but failed before resetting the
    // workspace leaves a conflicts-map workspace with no MERGE_HEAD. Re-running
    // `merge --abort` must still finish the abort (idempotent recovery).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (ours, _theirs) = dangling_merge_in_progress(&repo);

    // Simulate the half-aborted state: drop MERGE_HEAD, leave the partial-merge
    // workspace in place.
    repo.store()
        .delete_ref(MERGE_HEAD)
        .expect("drop merge head");
    assert!(repo.merge_head().expect("merge head").is_none());
    assert!(repo.is_dirty().expect("dirty"), "workspace still partial");

    // Re-abort recovers: workspace back to ours, clean, fsck-clean.
    repo.abort_merge().expect("recovering abort");
    assert!(!repo.is_dirty().expect("dirty"));
    assert_eq!(repo.head_commit().expect("head"), Some(ours));
    assert!(!fsck::check(&repo).expect("fsck").has_errors());
}

#[test]
fn commit_refuses_while_a_graph_violation_remains() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    dangling_merge_in_progress(&repo);

    // No repair: completing the merge must be refused by the re-validation.
    let txn = repo.begin_write().expect("begin");
    let err = txn.commit("finish", &[], None).expect_err("must refuse");
    assert!(matches!(err, GraphError::MergeState(_)), "got {err:?}");
}

#[test]
fn resolving_a_dangling_edge_by_deleting_it_completes_the_merge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (ours, theirs) = dangling_merge_in_progress(&repo);

    // Repair by dropping the dangling edge, then complete.
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_edge(&edge(1, 2)).expect("delete edge");
    let merge_commit = tx.commit("merge other", &[], None).expect("commit");

    // A two-parent merge commit landed; state cleared; graph is fsck-clean.
    let commit = repo
        .store()
        .read_commit(&merge_commit)
        .expect("read")
        .expect("commit");
    assert_eq!(commit.parents, vec![ours, theirs]);
    assert!(repo.merge_head().expect("merge head").is_none());
    assert!(!fsck::check(&repo).expect("fsck").has_errors());
}

#[test]
fn resolving_a_dangling_edge_by_restoring_the_endpoint_completes_the_merge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    dangling_merge_in_progress(&repo);

    // Repair by restoring the deleted endpoint node, then complete.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(2), &record(&[])).expect("restore node");
    tx.commit("merge other", &[], None).expect("commit");

    assert!(repo.merge_head().expect("merge head").is_none());
    let snap = repo.workspace_snapshot().expect("snapshot");
    assert!(snap.get_node(&node(2)).expect("get").is_some());
    assert!(!fsck::check(&repo).expect("fsck").has_errors());
}

#[test]
fn completion_re_validation_catches_a_merge_that_drops_a_required_property() {
    // acetone-36y: a cell-conflict merge whose auto-merge removes a required
    // property must not commit. Existence constraint on `email`; base has it;
    // theirs deletes it (a one-sided delete → auto-removed) while both sides
    // also edit `v` (the cell conflict that puts the merge in progress). After
    // resolving `v`, completion re-validation must reject the email-less node.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let def = LabelDef::new(
        vec!["id".to_string()],
        BTreeMap::new(),
        ["email".to_string()], // existence-required
        [],
    )
    .expect("label def");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Label {
        name: "N".into(),
        def,
    })
    .expect("schema");
    tx.put_node(
        &node(1),
        &record(&[("email", s("a@x")), ("v", Value::Int(0))]),
    )
    .expect("put");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    // theirs removes email and sets v=2.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(&[("v", Value::Int(2))]))
        .expect("put");
    tx.commit("theirs drops email", &[], None).expect("commit");

    repo.checkout_branch("main").expect("checkout");
    // ours keeps email and sets v=1 (the v conflict).
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(
        &node(1),
        &record(&[("email", s("a@x")), ("v", Value::Int(1))]),
    )
    .expect("put");
    tx.commit("ours sets v", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(_) => {}
        other => panic!("expected a cell conflict on v, got {other:?}"),
    }
    // Resolve the v conflict to ours (keeps email=a@x, v=1)... but the merge
    // already auto-removed email (one-sided delete by theirs), so the merged
    // node lacks the required property regardless of the v pick.
    repo.resolve_all(acetone_graph::repo::ResolveSide::Theirs)
        .expect("resolve");
    let txn = repo.begin_write().expect("begin");
    let err = txn
        .commit("merge other", &[], None)
        .expect_err("must refuse");
    assert!(
        matches!(err, GraphError::MergeState(_)),
        "completion must reject the missing-required-property graph, got {err:?}"
    );
}

#[test]
fn a_stale_merge_head_is_not_re_added_as_a_parent_and_is_cleared() {
    // Defensive (acetone-mws, m2): if a prior completion's MERGE_HEAD delete
    // failed, a MERGE_HEAD that is already an ancestor of the branch tip must
    // not turn the next ordinary commit into a spurious merge commit.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(&[])).expect("put");
    let head = tx.commit("base", &[], None).expect("commit");

    // Simulate a stale MERGE_HEAD pointing at the current tip (an ancestor of
    // itself), as a failed delete would leave behind.
    repo.store()
        .write_ref(MERGE_HEAD, None, &head)
        .expect("set stale merge head");

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(2), &record(&[])).expect("put");
    let commit = tx.commit("ordinary", &[], None).expect("commit");

    // The commit is an ordinary single-parent one (stale MERGE_HEAD ignored),
    // and MERGE_HEAD has been cleared.
    let c = repo
        .store()
        .read_commit(&commit)
        .expect("read")
        .expect("commit");
    assert_eq!(
        c.parents,
        vec![head],
        "stale MERGE_HEAD must not be a parent"
    );
    assert!(repo.merge_head().expect("merge head").is_none());
}
