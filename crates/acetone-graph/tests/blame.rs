//! Tests for node blame (acetone-14c.6): `Repository::blame` returns the
//! commits that changed a node's record, newest first, walking the
//! first-parent chain from HEAD.

use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_store::Hash;
use proptest::prelude::*;
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

/// Put/overwrite one node and commit; returns the commit.
fn put(repo: &Repository, id: u8, v: i64, message: &str) -> Hash {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(id), &record(v)).expect("put");
    tx.commit(message, &[], None).expect("commit")
}

#[test]
fn blame_lists_only_the_commits_that_change_the_node() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let c1 = put(&repo, 1, 10, "add 1");
    let _c2 = put(&repo, 2, 20, "add 2 (unrelated to 1)");
    let c3 = put(&repo, 1, 11, "change 1");

    // Newest first, skipping the unrelated commit.
    assert_eq!(repo.blame(&node(1)).expect("blame"), vec![c3, c1]);
    // A node never touched by later commits blames only to its introduction.
    assert_eq!(repo.blame(&node(2)).expect("blame").len(), 1);
}

#[test]
fn blame_of_an_absent_node_is_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    put(&repo, 1, 10, "add 1");
    assert!(repo.blame(&node(99)).expect("blame").is_empty());
}

#[test]
fn a_deletion_is_a_blamed_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let c1 = put(&repo, 1, 10, "add 1");
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(1)).expect("delete");
    let c2 = tx.commit("delete 1", &[], None).expect("commit");

    // The node is absent at HEAD, but both the deletion and the introduction
    // changed it.
    assert_eq!(repo.blame(&node(1)).expect("blame"), vec![c2, c1]);
}

#[test]
fn a_merge_attributes_a_branch_change_to_the_merge_commit() {
    // base sets 1=10; a branch changes 1=11; main is untouched; the merge
    // brings the branch value in, so blame credits the merge commit (its
    // record differs from its first parent, main) and the base.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = put(&repo, 1, 10, "base");
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    put(&repo, 1, 11, "change 1 on other");
    repo.checkout_branch("main").expect("checkout");
    // Advance main with an unrelated node so the merge is a real three-way.
    put(&repo, 2, 20, "add 2 on main");

    let merge_commit = match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Merged(h) => h,
        other => panic!("expected a clean merge, got {other:?}"),
    };

    let blame = repo.blame(&node(1)).expect("blame");
    assert_eq!(blame.first(), Some(&merge_commit), "newest is the merge");
    assert_eq!(blame.last(), Some(&base), "oldest is the introduction");
    assert_eq!(blame.len(), 2);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn blame_is_exactly_the_touching_commits(touches in prop::collection::vec(any::<bool>(), 1..8)) {
        // Each step commits a *fresh* value to node 1 (touch) or node 2 (not),
        // so every commit is a real change and node-1 touches are genuine
        // record changes. blame(1) must be exactly those commits, newest first.
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init(dir.path());
        let mut expected = Vec::new();
        let (mut v1, mut v2) = (0i64, 0i64);
        for touch1 in &touches {
            let commit = if *touch1 {
                v1 += 1;
                put(&repo, 1, v1, "touch 1")
            } else {
                v2 += 1;
                put(&repo, 2, v2, "touch 2")
            };
            if *touch1 {
                expected.push(commit);
            }
        }
        expected.reverse(); // blame is newest first
        prop_assert_eq!(repo.blame(&node(1)).expect("blame"), expected);
    }
}
