//! Integration tests for the tag lifecycle (acetone-ujsk, ADR-0059):
//! `Repository::{tags, create_tag, delete_tag}` — thin native tag commands
//! writing annotated tags at the graph's namespaced path, so short-name
//! `--at` resolution, `gc` ownership and `migrate` rewriting all manage
//! them. Deliberately mirrors the branch API (`branches`/`create_branch`/
//! `delete_branch`).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use acetone_graph::GraphError;
use acetone_graph::repo::{DEFAULT_BRANCH, InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_prolly::Hash;
use acetone_store::RefStore;

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

#[test]
fn create_tag_defaults_to_head_and_writes_an_annotated_tag() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let head = commit_one(&repo, "a");

    let target = repo
        .create_tag("v1", None, Some("first audited state"))
        .expect("create tag");
    assert_eq!(target, head, "create_tag returns the tagged commit");

    // The ref points at a fresh annotated tag OBJECT, not the commit…
    let ref_target = repo
        .store()
        .read_ref("refs/tags/v1")
        .expect("read")
        .expect("tag ref present");
    assert_ne!(ref_target, head, "annotated: ref names a tag object");
    let tag = repo
        .store()
        .read_tag(&ref_target)
        .expect("read tag")
        .expect("a tag object");
    assert_eq!(tag.target, head);
    assert_eq!(tag.name, "v1");
    assert_eq!(tag.message.trim_end(), "first audited state");
    assert!(!tag.signed);
    let tagger = tag.tagger.expect("tagger recorded");
    assert_eq!(
        (tagger.name.as_str(), tagger.email.as_str()),
        ("acetone", "acetone@acetone.invalid"),
        "same neutral placeholder identity as commits (ADR-0059; real \
         identity is acetone-gid)"
    );

    // …and the short name resolves through the peel to the commit.
    assert_eq!(repo.resolve_commit("v1").expect("resolve"), head);
}

#[test]
fn create_tag_message_defaults_to_the_tag_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_one(&repo, "a");

    repo.create_tag("v2", None, None).expect("create tag");
    let ref_target = repo
        .store()
        .read_ref("refs/tags/v2")
        .expect("read")
        .expect("present");
    let tag = repo
        .store()
        .read_tag(&ref_target)
        .expect("read tag")
        .expect("a tag object");
    assert_eq!(tag.message.trim_end(), "v2");
}

#[test]
fn create_tag_takes_a_refspec_like_branch_creation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let first = commit_one(&repo, "a");
    let second = commit_one(&repo, "b");

    // By hex hash.
    let target = repo
        .create_tag("at-first", Some(&first.to_hex()), None)
        .expect("tag by hash");
    assert_eq!(target, first);
    assert_eq!(repo.resolve_commit("at-first").expect("resolve"), first);

    // By branch short name.
    let target = repo
        .create_tag("at-branch", Some(DEFAULT_BRANCH), None)
        .expect("tag by branch");
    assert_eq!(target, second);

    // By another tag's short name (peels through the tag object).
    let target = repo
        .create_tag("at-tag", Some("at-first"), None)
        .expect("tag by tag");
    assert_eq!(target, first);
}

#[test]
fn duplicate_tag_creation_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_one(&repo, "a");

    repo.create_tag("v1", None, None).expect("create tag");
    assert!(matches!(
        repo.create_tag("v1", None, None),
        Err(GraphError::TagExists { name }) if name == "v1"
    ));
}

#[test]
fn create_tag_on_an_unborn_head_reports_no_current_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    assert!(matches!(
        repo.create_tag("v1", None, None),
        Err(GraphError::NoCurrentBranch)
    ));
}

#[test]
fn create_tag_rejects_an_invalid_name_without_leaving_debris() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_one(&repo, "a");

    for bad in ["", "a..b", "a b", "end/", "a\nb"] {
        assert!(
            repo.create_tag(bad, None, None).is_err(),
            "tag name {bad:?} must be rejected"
        );
    }
    assert_eq!(repo.tags().expect("tags"), vec![], "no tag was created");
}

#[test]
fn tags_lists_short_names_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_one(&repo, "a");

    repo.create_tag("v2", None, None).expect("tag");
    repo.create_tag("v1", None, None).expect("tag");

    let tags = repo.tags().expect("tags");
    let names: Vec<&str> = tags.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["v1", "v2"], "short names, name order");
    // Each listed hash is the ref's raw target — the annotated tag object.
    for (name, hash) in &tags {
        let tag = repo
            .store()
            .read_tag(hash)
            .expect("read")
            .expect("tag object");
        assert_eq!(&tag.name, name);
    }
}

#[test]
fn delete_tag_removes_the_ref_and_reports_the_former_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_one(&repo, "a");

    repo.create_tag("v1", None, None).expect("tag");
    let ref_target = repo
        .store()
        .read_ref("refs/tags/v1")
        .expect("read")
        .expect("present");

    let was = repo.delete_tag("v1").expect("delete");
    assert_eq!(was, ref_target, "reports the raw ref target, as git does");
    assert_eq!(
        repo.store().read_ref("refs/tags/v1").expect("read"),
        None,
        "ref removed"
    );
    assert!(matches!(
        repo.delete_tag("v1"),
        Err(GraphError::NoSuchTag { name }) if name == "v1"
    ));
    // Ref plumbing only: the commit is still reachable by hash.
    assert!(repo.resolve_commit("v1").is_err());
}

/// Run `git -C <dir> <args>`, asserting success, returning trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Code Dev",
            "-c",
            "user.email=dev@example.invalid",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

#[test]
fn co_tenant_tags_land_in_the_graph_namespace_not_the_code_repos() {
    // The load-bearing property (ADR-0059): creation goes through
    // namespace.tag_ref, so the short name resolves, and the code repo's
    // own refs/tags stays untouched.
    let dir = tempfile::tempdir().expect("tmp");
    let project = dir.path().join("project");
    std::fs::create_dir(&project).expect("mkdir");
    git(&project, &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(project.join("code.txt"), "code").expect("write");
    git(&project, &["add", "code.txt"]);
    git(&project, &["commit", "-m", "code: initial"]);

    let graph =
        Repository::init_co_tenant(&project, "inventory", InitOptions::default()).expect("init");
    let mut tx = graph.begin_write().expect("begin");
    tx.put_node(
        &NodeKey::new("N", vec![Value::Int(1)]).expect("key"),
        &NodeRecord::new([], BTreeMap::new()),
    )
    .expect("put");
    tx.commit("graph: first", &[], None).expect("commit");
    let head = graph.head_commit().expect("head").expect("commit");

    let target = graph.create_tag("v1", None, None).expect("tag");
    assert_eq!(target, head);

    // Physically under the graph's namespace…
    assert!(
        graph
            .store()
            .read_ref("refs/tags/acetone/inventory/v1")
            .expect("read")
            .is_some(),
        "tag lives at the namespaced path"
    );
    // …not in the code repository's tag namespace.
    assert_eq!(
        graph.store().read_ref("refs/tags/v1").expect("read"),
        None,
        "the code repo's refs/tags is untouched"
    );

    // The short name resolves for the graph; listing shows the short name.
    assert_eq!(graph.resolve_commit("v1").expect("resolve"), head);
    let names: Vec<String> = graph
        .tags()
        .expect("tags")
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["v1"]);

    // A CODE tag with the same short name neither collides nor leaks in.
    git(&project, &["tag", "code-v1"]);
    let names: Vec<String> = graph
        .tags()
        .expect("tags")
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, ["v1"], "code tags stay invisible to the graph");

    // git sees the graph tag as an ordinary (annotated) tag it can verify,
    // list and delete — the manual's "drop to git" equivalence.
    let listed = git(&project, &["tag", "--list", "acetone/inventory/*"]);
    assert_eq!(listed, "acetone/inventory/v1");
}
