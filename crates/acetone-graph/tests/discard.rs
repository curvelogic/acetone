//! Workspace discard (acetone-omk): `Repository::discard_workspace` throws
//! away staged but uncommitted changes, resetting the workspace to the
//! checked-out commit's committed state, and refuses while a merge is in
//! progress (`merge --abort` is that state's way out).

use std::collections::BTreeMap;
use std::path::Path;

use acetone_graph::GraphError;
use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_store::RefStore;

const MERGE_HEAD: &str = "refs/worktree/acetone/merge-head";

fn node(id: i64) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(id)]).expect("valid key")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(v))]))
}

fn edge(a: i64, b: i64) -> EdgeKey {
    EdgeKey::new(node(a), "R", node(b), Value::Null).expect("valid edge key")
}

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

/// A committed graph of nodes 0..3 with edges 0->1 and 1->2.
fn committed(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    for id in 0..3 {
        tx.put_node(&node(id), &record(0)).expect("put");
    }
    tx.put_edge(&edge(0, 1), &EdgeRecord::new(BTreeMap::new()))
        .expect("edge");
    tx.put_edge(&edge(1, 2), &EdgeRecord::new(BTreeMap::new()))
        .expect("edge");
    tx.commit("base", &[], None).expect("commit");
}

#[test]
fn discard_resets_a_dirty_workspace_to_the_committed_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    committed(&repo);
    let head = repo.head_commit().expect("head").expect("born");
    // Clean right after the commit, so this is the commit's manifest.
    let committed_manifest = repo.workspace_manifest().expect("manifest");

    // Stage changes touching every primary map: a new node, an edited node,
    // a deleted edge and a new edge (so edges_rev moves too), then save.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(9), &record(9)).expect("put");
    tx.put_node(&node(0), &record(7)).expect("edit");
    tx.delete_edge(&edge(0, 1)).expect("delete edge");
    tx.put_edge(&edge(2, 0), &EdgeRecord::new(BTreeMap::new()))
        .expect("edge");
    tx.save().expect("save");
    assert!(repo.is_dirty().expect("dirty"));

    assert!(
        repo.discard_workspace().expect("discard"),
        "something discarded"
    );
    assert!(!repo.is_dirty().expect("dirty"));
    // The whole manifest equals the commit's, derived maps included
    // (Invariant 5): edges_rev and indexes come back with it, not rebuilt.
    let ws = repo.workspace_manifest().expect("workspace");
    assert_eq!(ws.encode(), committed_manifest.encode());
    assert_eq!(
        repo.head_commit().expect("head"),
        Some(head),
        "HEAD unmoved"
    );
}

#[test]
fn discard_on_a_clean_workspace_reports_nothing_discarded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    committed(&repo);
    assert!(!repo.discard_workspace().expect("discard"));
    assert!(!repo.is_dirty().expect("dirty"));
}

#[test]
fn discard_on_an_unborn_branch_returns_to_the_empty_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(1)).expect("put");
    tx.save().expect("save");
    assert!(repo.is_dirty().expect("dirty"));

    assert!(repo.discard_workspace().expect("discard"));
    assert!(!repo.is_dirty().expect("dirty"));
    assert!(repo.head_commit().expect("head").is_none(), "still unborn");
}

#[test]
fn discard_is_refused_while_a_merge_is_in_progress() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    committed(&repo);
    let base = repo.head_commit().expect("head").expect("born");
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout other");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(0), &record(2)).expect("put");
    tx.commit("theirs", &[], None).expect("commit");
    repo.checkout_branch("main").expect("checkout main");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(0), &record(1)).expect("put");
    tx.commit("ours", &[], None).expect("commit");

    let outcome = repo.merge("other", "merge").expect("merge");
    assert!(matches!(outcome, MergeOutcome::Conflicts(_)));
    let before = repo.workspace_manifest().expect("ws").encode();

    match repo.discard_workspace() {
        Err(GraphError::DiscardDuringMerge) => {}
        other => panic!("expected DiscardDuringMerge, got {other:?}"),
    }
    // Nothing moved: the partial merge, its conflicts and MERGE_HEAD stand.
    assert_eq!(repo.workspace_manifest().expect("ws").encode(), before);
    assert!(repo.merge_head().expect("merge head").is_some());

    // The advertised way out works, and discard is then a clean no-op.
    repo.abort_merge().expect("abort");
    assert!(!repo.discard_workspace().expect("discard"));
}

#[test]
fn discard_is_refused_over_a_stray_merge_head() {
    // MERGE_HEAD alone (a conflict-free workspace) still marks a merge in
    // progress: a discard that left it would let the next commit record a
    // merge.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    committed(&repo);
    let head = repo.head_commit().expect("head").expect("born");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(9), &record(9)).expect("put");
    tx.save().expect("save");
    repo.store()
        .write_ref(MERGE_HEAD, None, &head)
        .expect("plant MERGE_HEAD");

    assert!(matches!(
        repo.discard_workspace(),
        Err(GraphError::DiscardDuringMerge)
    ));
    assert!(repo.is_dirty().expect("dirty"), "staged work kept");
}

#[test]
fn discard_is_refused_while_another_writer_holds_the_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    committed(&repo);
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(9), &record(9)).expect("put");
    match repo.discard_workspace() {
        Err(GraphError::Locked { .. }) => {}
        other => panic!("expected Locked, got {other:?}"),
    }
    tx.save().expect("the writer's save is unaffected");
    assert!(repo.discard_workspace().expect("discard"));
}

/// A detached worktree's discard (and the streaming-import cleanup's
/// `reset_workspace_to_head`) resets to the CHECKED-OUT commit, as
/// `is_dirty` judges it, not to the empty graph: a detached HEAD has no
/// current branch, and resolving through the branch would stage the
/// deletion of the whole graph.
#[test]
fn discard_in_a_detached_worktree_resets_to_its_checked_out_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let main_git = dir.path().join("g.git");
    let repo = init(dir.path());
    committed(&repo);
    let head = repo.head_commit().expect("head").expect("born");
    let committed_manifest = repo.workspace_manifest().expect("ws").encode();

    let wt = dir.path().join("wt");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_git)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(head.to_hex())
        .status()
        .expect("run git worktree add");
    assert!(status.success(), "git worktree add failed");
    let wt_repo = Repository::open(&wt).expect("open worktree");
    assert_eq!(wt_repo.current_branch().expect("branch"), None, "detached");

    for reset in [
        |r: &Repository| r.discard_workspace().map(|_| ()),
        |r: &Repository| r.reset_workspace_to_head(),
    ] {
        let mut tx = wt_repo.begin_write().expect("begin");
        tx.put_node(&node(9), &record(9)).expect("put");
        tx.save().expect("save");
        assert!(wt_repo.is_dirty().expect("dirty"));

        reset(&wt_repo).expect("reset");
        assert!(!wt_repo.is_dirty().expect("dirty"));
        assert_eq!(
            wt_repo.workspace_manifest().expect("ws").encode(),
            committed_manifest,
            "reset to the checked-out commit, not the empty graph"
        );
    }
}
