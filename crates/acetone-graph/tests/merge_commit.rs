//! Integration tests for the commit-graph merge wrapper
//! ([`Repository::merge`], acetone-14c.2 part 2): merge-base resolution,
//! fast-forward and up-to-date short-circuits, two-parent merge commits for
//! a clean three-way merge, and conflict reporting that leaves the
//! repository untouched. Determinism (Invariant #4) is checked on the
//! merged manifest *content* — git commit timestamps are wall-clock, so the
//! merge commit's own hash is not reproducible, but its tree is.

use acetone_graph::merge::{MergeConflict, MergeOutcome};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_graph::{GraphError, fsck};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_store::CommitStore;
use std::collections::BTreeMap;
use std::path::Path;

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_string(), Value::Int(v))]))
}

/// Put/overwrite one node and commit; returns the new commit hash.
fn commit_node(repo: &Repository, id: u8, v: i64, message: &str) -> acetone_store::Hash {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(id), &record(v)).expect("put");
    tx.commit(message, &[], None).expect("commit")
}

/// Delete one node and commit; returns the new commit hash.
fn delete_node(repo: &Repository, id: u8, message: &str) -> acetone_store::Hash {
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(id)).expect("delete");
    tx.commit(message, &[], None).expect("commit")
}

/// The encoded manifest of the version at `refspec`.
fn manifest_bytes(repo: &Repository, refspec: &str) -> Vec<u8> {
    repo.snapshot(refspec)
        .expect("snapshot")
        .manifest()
        .encode()
}

#[test]
fn merging_an_ancestor_is_up_to_date() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");
    repo.create_branch("old", Some(&base.to_hex()))
        .expect("branch");
    let head = commit_node(&repo, 2, 20, "advance main");

    // `old` points at `base`, an ancestor of the current head.
    match repo.merge("old", "merge old").expect("merge") {
        MergeOutcome::AlreadyUpToDate => {}
        other => panic!("expected AlreadyUpToDate, got {other:?}"),
    }
    // The branch did not move and the workspace is not left dirty.
    assert_eq!(repo.head_commit().expect("head"), Some(head));
    assert!(!repo.is_dirty().expect("dirty"));
}

#[test]
fn merging_a_descendant_fast_forwards() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");
    // `feature` diverges ahead of main by one commit.
    repo.create_branch("feature", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("feature").expect("checkout");
    let feature_head = commit_node(&repo, 2, 20, "feature work");
    repo.checkout_branch("main").expect("checkout main");
    assert_eq!(repo.head_commit().expect("head"), Some(base));

    match repo.merge("feature", "merge feature").expect("merge") {
        MergeOutcome::FastForward(h) => assert_eq!(h, feature_head),
        other => panic!("expected FastForward, got {other:?}"),
    }
    // main now points at feature's head, no merge commit created.
    assert_eq!(repo.head_commit().expect("head"), Some(feature_head));
    let commit = repo
        .store()
        .read_commit(&feature_head)
        .expect("read")
        .unwrap();
    assert_eq!(
        commit.parents,
        vec![base],
        "fast-forward adds no merge commit"
    );
    // The workspace matches the fast-forwarded state.
    assert!(!repo.is_dirty().expect("dirty"));
    assert_eq!(
        repo.workspace_manifest().expect("ws").encode(),
        manifest_bytes(&repo, "feature")
    );
}

#[test]
fn clean_three_way_merge_creates_a_two_parent_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");
    // theirs (branch `other` at base) adds node 3.
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let theirs = commit_node(&repo, 3, 30, "add 3 on other");
    // ours (main) adds node 2.
    repo.checkout_branch("main").expect("checkout");
    let ours = commit_node(&repo, 2, 20, "add 2 on main");

    let merge_commit = match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Merged(h) => h,
        other => panic!("expected Merged, got {other:?}"),
    };

    // The branch advanced to the merge commit, which has both tips as parents.
    assert_eq!(repo.head_commit().expect("head"), Some(merge_commit));
    let commit = repo
        .store()
        .read_commit(&merge_commit)
        .expect("read")
        .unwrap();
    assert_eq!(
        commit.parents,
        vec![ours, theirs],
        "merge commit records [ours, theirs] in order"
    );

    // Content: the merged version is the union {1,2,3}, byte-identical to a
    // direct build (Invariant #1). Compare against an oracle repo.
    let odir = tempfile::tempdir().expect("tempdir");
    let orepo = init(odir.path());
    let mut tx = orepo.begin_write().expect("begin");
    for (id, v) in [(1, 10), (2, 20), (3, 30)] {
        tx.put_node(&node(id), &record(v)).expect("put");
    }
    let ocommit = tx.commit("oracle", &[], None).expect("commit");
    assert_eq!(
        manifest_bytes(&repo, &merge_commit.to_hex()),
        manifest_bytes(&orepo, &ocommit.to_hex()),
        "merged manifest equals a direct build of the union"
    );

    // The workspace tracks the merge, and the repository verifies clean.
    assert!(!repo.is_dirty().expect("dirty"));
    let report = fsck(&repo).expect("fsck");
    assert!(
        !report.has_errors(),
        "fsck must be clean after a clean merge: {:?}",
        report.errors().collect::<Vec<_>>()
    );
}

#[test]
fn conflicting_merge_reports_conflicts_and_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");
    // Both sides change node 1 to different values -> a genuine conflict.
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    commit_node(&repo, 1, 12, "other sets 1=12");
    repo.checkout_branch("main").expect("checkout");
    let ours = commit_node(&repo, 1, 11, "main sets 1=11");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            let MergeConflict::Cell(cell) = &conflicts[0] else {
                panic!("expected a cell conflict, got {:?}", conflicts[0]);
            };
            assert_eq!(cell.key, node(1).encode().expect("encode"));
        }
        other => panic!("expected Conflicts, got {other:?}"),
    }
    // A conflicted merge writes no commit and leaves the workspace clean on
    // our tip (persisting the conflicts map is acetone-14c.4).
    assert_eq!(repo.head_commit().expect("head"), Some(ours));
    assert!(!repo.is_dirty().expect("dirty"));
}

#[test]
fn disjoint_deletions_and_additions_merge_cleanly() {
    // theirs deletes a node; ours adds one — disjoint, so a clean merge.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(10)).expect("put");
    tx.put_node(&node(2), &record(20)).expect("put");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    delete_node(&repo, 2, "other deletes 2");
    repo.checkout_branch("main").expect("checkout");
    commit_node(&repo, 3, 30, "main adds 3");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Merged(h) => {
            // Merged version holds {1,3}: 2 was deleted, 3 was added.
            let snap = repo.snapshot(&h.to_hex()).expect("snapshot");
            assert!(snap.get_node(&node(1)).expect("get").is_some());
            assert!(snap.get_node(&node(2)).expect("get").is_none());
            assert!(snap.get_node(&node(3)).expect("get").is_some());
        }
        other => panic!("expected Merged, got {other:?}"),
    }
}

#[test]
fn merge_refuses_a_dirty_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    commit_node(&repo, 2, 20, "other work");
    repo.checkout_branch("main").expect("checkout");

    // Stage an uncommitted change on main.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(9), &record(90)).expect("put");
    tx.save().expect("save");
    assert!(repo.is_dirty().expect("dirty"));

    let err = repo.merge("other", "merge other").expect_err("must refuse");
    assert!(
        matches!(err, GraphError::DirtyWorkspace),
        "expected DirtyWorkspace, got {err:?}"
    );
}

#[test]
fn merge_is_content_deterministic_across_repositories() {
    // The same base/ours/theirs edits in two independent repositories yield
    // the same merged manifest (Invariant #4): content, not commit identity.
    fn merged_manifest(seed: u8) -> Vec<u8> {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init(dir.path());
        let base = commit_node(&repo, 1, 10, "base");
        repo.create_branch("other", Some(&base.to_hex()))
            .expect("branch");
        repo.checkout_branch("other").expect("checkout");
        commit_node(&repo, 3, 30, "add 3");
        repo.checkout_branch("main").expect("checkout");
        commit_node(&repo, 2, 20, "add 2");
        // Vary a no-op to prove the merge ignores incidental history: a
        // second identical repo built independently must still match.
        let _ = seed;
        match repo.merge("other", "merge other").expect("merge") {
            MergeOutcome::Merged(h) => manifest_bytes(&repo, &h.to_hex()),
            other => panic!("expected Merged, got {other:?}"),
        }
    }
    assert_eq!(merged_manifest(0), merged_manifest(1));
}
