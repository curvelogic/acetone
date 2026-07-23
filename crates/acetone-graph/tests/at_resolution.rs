//! Integration tests for refspec resolution (`Repository::resolve_commit`,
//! acetone-lqq): tag short names resolve like branch short names, annotated
//! tags peel to their target commit, and precedence follows git
//! (gitrevisions "first match wins": exact `refs/…` path, then
//! `refs/tags/<name>`, then `refs/heads/<name>`, then a commit hash) — so a
//! name that is both a tag and a branch resolves to the tag, exactly as
//! `git rev-parse` would.

use std::path::Path;

use acetone_graph::GraphError;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_prolly::Hash;
use acetone_store::{RefStore, StoreError};

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

fn node(label: &str, key: &str) -> NodeKey {
    NodeKey::new(label, vec![Value::String(key.to_owned())]).expect("valid")
}

/// Insert one node and commit; returns the new head commit.
fn commit_one(repo: &Repository, key: &str) -> Hash {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", key), &NodeRecord::new([], Default::default()))
        .expect("put node");
    tx.commit(&format!("add {key}"), &[], None).expect("commit");
    repo.head_commit().expect("head").expect("a commit")
}

/// Create an annotated tag `name` on `target` (a commit or tag object hash)
/// with the system git binary (tests may shell out to git; library code
/// never does).
fn git_tag_annotated(repo: &Repository, name: &str, target: &Hash) {
    let git_dir = repo.store().common_dir().to_owned();
    let status = std::process::Command::new("git")
        .args(["-c", "user.name=at-test", "-c", "user.email=at@test"])
        .arg("-C")
        .arg(&git_dir)
        .args(["tag", "-a", name, "-m", "annotated tag"])
        .arg(target.to_hex())
        .status()
        .expect("run git tag");
    assert!(status.success(), "git tag -a failed");
}

/// The tag object hash a tag ref points at.
fn tag_object(repo: &Repository, name: &str) -> Hash {
    repo.store()
        .read_ref(&format!("refs/tags/{name}"))
        .expect("read tag ref")
        .expect("tag ref present")
}

#[test]
fn a_lightweight_tag_short_name_resolves_to_its_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let head = commit_one(&repo, "a");

    repo.store()
        .write_ref("refs/tags/v1", None, &head)
        .expect("tag ref");
    assert_eq!(
        repo.resolve_commit("v1").expect("short tag name"),
        head,
        "a lightweight tag must resolve by its short name"
    );
    // The full path keeps working.
    assert_eq!(
        repo.resolve_commit("refs/tags/v1").expect("full path"),
        head
    );
}

#[test]
fn an_annotated_tag_peels_to_its_target_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let head = commit_one(&repo, "a");

    git_tag_annotated(&repo, "v2", &head);
    assert_ne!(
        tag_object(&repo, "v2"),
        head,
        "an annotated tag ref names a tag object, not the commit"
    );
    // Short name: expanded to refs/tags/v2 AND peeled to the commit.
    assert_eq!(repo.resolve_commit("v2").expect("short annotated"), head);
    // Full path: the originally reported failure — must peel, not error
    // with "object … is a tag, expected a commit".
    assert_eq!(
        repo.resolve_commit("refs/tags/v2").expect("full annotated"),
        head
    );
}

#[test]
fn a_nested_annotated_tag_chain_peels_all_the_way_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let head = commit_one(&repo, "a");

    git_tag_annotated(&repo, "inner", &head);
    let inner = tag_object(&repo, "inner");
    git_tag_annotated(&repo, "outer", &inner);
    assert_eq!(repo.resolve_commit("outer").expect("nested"), head);
}

#[test]
fn a_name_that_is_both_tag_and_branch_resolves_to_the_tag() {
    // Git parity (gitrevisions): refs/tags/<name> is tried before
    // refs/heads/<name>, so the tag wins. This test pins the documented
    // precedence; the full ref paths keep both reachable unambiguously.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let first = commit_one(&repo, "a");
    repo.create_branch("release", None).expect("branch");
    let second = commit_one(&repo, "b");
    repo.store()
        .write_ref("refs/tags/release", None, &second)
        .expect("tag ref");

    assert_eq!(
        repo.resolve_commit("release").expect("ambiguous"),
        second,
        "tags resolve before branches (git parity)"
    );
    assert_eq!(
        repo.resolve_commit("refs/heads/release")
            .expect("branch path"),
        first,
        "the branch stays reachable by its full ref path"
    );
    assert_eq!(
        repo.resolve_commit("refs/tags/release").expect("tag path"),
        second
    );
}

#[test]
fn a_hex_address_of_a_tag_object_resolves_to_its_commit() {
    // `--at <hash-of-annotated-tag-object>` peels too, as git does.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let head = commit_one(&repo, "a");
    git_tag_annotated(&repo, "v3", &head);
    let tag = tag_object(&repo, "v3");

    assert_eq!(repo.resolve_commit(&tag.to_hex()).expect("hex tag"), head);
}

#[test]
fn an_over_deep_tag_chain_is_a_typed_error_not_a_panic() {
    // A chain deeper than the peel cap must surface as a typed corrupt-store
    // error — never a panic, and never a silent fall-through to some other
    // resolution arm.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let head = commit_one(&repo, "a");

    git_tag_annotated(&repo, "chain-0", &head);
    let mut prev = tag_object(&repo, "chain-0");
    // MAX_TAG_PEEL_DEPTH is 32: 33 tag objects in a row exceed it.
    for i in 1..33 {
        let name = format!("chain-{i}");
        git_tag_annotated(&repo, &name, &prev);
        prev = tag_object(&repo, &name);
    }

    let err = repo.resolve_commit("chain-32").expect_err("too deep");
    assert!(
        matches!(err, GraphError::Store(StoreError::Corrupt { .. })),
        "expected a typed corrupt-chain error, got {err:?}"
    );
}

#[test]
fn branch_short_names_and_absent_names_are_unchanged() {
    // The pre-existing arms still hold: a branch short name resolves, and a
    // name that is nothing at all is an UnresolvedRefspec.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let head = commit_one(&repo, "a");

    assert_eq!(repo.resolve_commit("main").expect("branch"), head);
    let err = repo.resolve_commit("no-such-name").expect_err("absent");
    assert!(
        matches!(err, GraphError::UnresolvedRefspec { ref refspec } if refspec == "no-such-name"),
        "expected UnresolvedRefspec, got {err:?}"
    );
}
