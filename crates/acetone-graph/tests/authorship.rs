//! Caller-supplied authorship through the library (acetone-gid, ADR-0079):
//! `Transaction::commit`, `Repository::merge_with_author` and
//! `Repository::create_tag_with_tagger` stamp the given identity, the
//! committer stays acetone's neutral signature, `None` means the neutral
//! signature (the library never reads an ambient identity), and an identity
//! git cannot store faithfully is refused with a typed error before
//! anything is written.

use std::collections::BTreeMap;
use std::path::Path;

use acetone_graph::GraphError;
use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_store::{CommitStore, Hash, RefStore, Signature, StoreError};

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn commit_node(repo: &Repository, id: u8, author: Option<Signature>) -> Hash {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(
        &node(id),
        &NodeRecord::new([], BTreeMap::from([("v".into(), Value::Int(1))])),
    )
    .expect("put");
    tx.commit(&format!("add {id}"), &[], author)
        .expect("commit")
}

fn ada() -> Signature {
    Signature {
        name: "Ada Lovelace".into(),
        email: "ada@example.com".into(),
    }
}

/// The header-injection attempt each surface must refuse: a `>` and a
/// newline would let the email forge a further object header.
fn forged() -> Signature {
    Signature {
        name: "Eve".into(),
        email: "eve@example.com>\ncommitter Mallory <m@example.com".into(),
    }
}

fn assert_neutral_committer(repo: &Repository, id: &Hash) {
    let commit = repo.store().read_commit(id).expect("read").expect("commit");
    let neutral = Signature::default();
    assert_eq!(commit.committer.name, neutral.name);
    assert_eq!(commit.committer.email, neutral.email);
}

#[test]
fn commit_records_the_caller_author_and_a_neutral_committer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let id = commit_node(&repo, 1, Some(ada()));
    let commit = repo
        .store()
        .read_commit(&id)
        .expect("read")
        .expect("commit");
    assert_eq!(commit.author.name, "Ada Lovelace");
    assert_eq!(commit.author.email, "ada@example.com");
    assert_neutral_committer(&repo, &id);
}

#[test]
fn commit_without_an_author_is_neutral() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let id = commit_node(&repo, 1, None);
    let commit = repo
        .store()
        .read_commit(&id)
        .expect("read")
        .expect("commit");
    assert_eq!(commit.author.name, Signature::default().name);
    assert_eq!(commit.author.email, Signature::default().email);
}

#[test]
fn commit_refuses_an_injected_author_before_applying_anything() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let head = commit_node(&repo, 1, None);
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(2), &NodeRecord::new([], BTreeMap::new()))
        .expect("put");
    let err = tx.commit("evil", &[], Some(forged())).expect_err("refused");
    assert!(
        matches!(err, GraphError::Store(StoreError::InvalidSignature { .. })),
        "{err:?}"
    );
    assert_eq!(repo.head_commit().expect("head"), Some(head));
    assert!(
        !repo.is_dirty().expect("dirty"),
        "staged writes not applied"
    );
}

/// main and `other` diverge from a common base, so merging `other` into
/// main is a clean three-way merge that writes a merge commit.
fn diverged(repo: &Repository) {
    let base = commit_node(repo, 1, None);
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    commit_node(repo, 3, None);
    repo.checkout_branch("main").expect("checkout");
    commit_node(repo, 2, None);
}

#[test]
fn clean_merge_commit_records_the_caller_author() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    diverged(&repo);
    let merged = match repo
        .merge_with_author("other", "merge other", Some(ada()))
        .expect("merge")
    {
        MergeOutcome::Merged(h) => h,
        other => panic!("expected Merged, got {other:?}"),
    };
    let commit = repo
        .store()
        .read_commit(&merged)
        .expect("read")
        .expect("commit");
    assert_eq!(commit.parents.len(), 2);
    assert_eq!(commit.author.name, "Ada Lovelace");
    assert_eq!(commit.author.email, "ada@example.com");
    assert_neutral_committer(&repo, &merged);
}

#[test]
fn plain_merge_stays_neutral() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    diverged(&repo);
    let MergeOutcome::Merged(merged) = repo.merge("other", "m").expect("merge") else {
        panic!("expected Merged");
    };
    let commit = repo.store().read_commit(&merged).expect("r").expect("c");
    assert_eq!(commit.author.name, Signature::default().name);
}

#[test]
fn merge_refuses_an_injected_author_before_merging() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    diverged(&repo);
    let head = repo.head_commit().expect("head");
    let err = repo
        .merge_with_author("other", "m", Some(forged()))
        .expect_err("refused");
    assert!(
        matches!(err, GraphError::Store(StoreError::InvalidSignature { .. })),
        "{err:?}"
    );
    assert_eq!(repo.head_commit().expect("head"), head);
    assert!(repo.merge_head().expect("merge head").is_none());
}

#[test]
fn annotated_tag_records_the_caller_tagger() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    commit_node(&repo, 1, None);
    repo.create_tag_with_tagger("v1", None, Some("audited"), Some(ada()))
        .expect("tag");
    let tag_id = repo
        .store()
        .read_ref("refs/tags/v1")
        .expect("read")
        .expect("tag ref");
    let tag = repo.store().read_tag(&tag_id).expect("read").expect("tag");
    let tagger = tag.tagger.expect("tagger");
    assert_eq!(tagger.name, "Ada Lovelace");
    assert_eq!(tagger.email, "ada@example.com");
}

#[test]
fn tag_refuses_an_injected_tagger_and_writes_no_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    commit_node(&repo, 1, None);
    let err = repo
        .create_tag_with_tagger("v1", None, None, Some(forged()))
        .expect_err("refused");
    assert!(
        matches!(err, GraphError::Store(StoreError::InvalidSignature { .. })),
        "{err:?}"
    );
    assert!(
        repo.store()
            .read_ref("refs/tags/v1")
            .expect("read")
            .is_none()
    );
}
