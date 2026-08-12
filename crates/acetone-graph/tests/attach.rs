//! `Repository::attach_co_tenant` (acetone-gufe): reattach a cloned
//! co-tenant graph — the three-command git dance every consumer used to
//! embed, done natively, idempotently, sharing the ref shape with
//! init/open through `GraphRefNamespace`.

use std::path::Path;
use std::process::Command;

use acetone_graph::GraphError;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;

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

fn node(key: i64) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(key)]).expect("key")
}

/// A code repo with one commit, a co-tenant graph `g` with one committed
/// node, cloned to `clone/` — the exact state `attach` exists for.
fn cloned_co_tenant() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tmp");
    let origin = dir.path().join("origin");
    std::fs::create_dir(&origin).expect("mkdir");
    git(&origin, &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(origin.join("code.txt"), "code").expect("write");
    git(&origin, &["add", "code.txt"]);
    git(&origin, &["commit", "-m", "code"]);

    let repo = Repository::init_co_tenant(&origin, "g", InitOptions::default()).expect("init g");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &NodeRecord::new([], Default::default()))
        .expect("put");
    tx.commit("add 1", &[], None).expect("commit");
    drop(repo);

    let clone = dir.path().join("clone");
    git(
        dir.path(),
        &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()],
    );
    (dir, clone)
}

#[test]
fn attach_makes_a_cloned_graph_readable_and_is_idempotent() {
    let (_dir, clone) = cloned_co_tenant();

    // Before attach: the graph is invisible (no marker in the clone).
    assert!(
        Repository::open_graph(&clone, "g").is_err(),
        "a fresh clone has no local graph state"
    );

    // Discovery: with no name given, the sole candidate attaches.
    let outcome = Repository::attach_co_tenant(&clone, None).expect("attach");
    assert_eq!(outcome.graph, "g");
    assert!(outcome.marker_written);
    assert!(outcome.head_set);
    assert_eq!(outcome.branches_created, vec!["main".to_owned()]);

    // The graph now reads: the committed node is visible.
    let repo = Repository::open_graph(&clone, "g").expect("open attached");
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    assert!(
        snapshot.get_node(&node(1)).expect("get").is_some(),
        "the cloned graph's committed state reads after attach"
    );
    drop(repo);

    // Idempotent: a second attach changes nothing and says so.
    let again = Repository::attach_co_tenant(&clone, Some("g")).expect("re-attach");
    assert!(!again.marker_written, "{again:?}");
    assert!(!again.head_set, "{again:?}");
    assert!(again.branches_created.is_empty(), "{again:?}");
}

#[test]
fn attach_refuses_when_nothing_is_attachable() {
    let dir = tempfile::tempdir().expect("tmp");
    let plain = dir.path().join("plain");
    std::fs::create_dir(&plain).expect("mkdir");
    git(&plain, &["-c", "init.defaultBranch=main", "init"]);
    std::fs::write(plain.join("x"), "x").expect("write");
    git(&plain, &["add", "x"]);
    git(&plain, &["commit", "-m", "x"]);
    let clone = dir.path().join("clone");
    git(
        dir.path(),
        &["clone", plain.to_str().unwrap(), clone.to_str().unwrap()],
    );

    match Repository::attach_co_tenant(&clone, None) {
        Err(GraphError::NoAttachableGraph) => {}
        other => panic!("nothing to attach must refuse typed: {other:?}"),
    }
    // Naming a graph that is not on the remote refuses too.
    match Repository::attach_co_tenant(&clone, Some("g")) {
        Err(GraphError::NoAttachableGraph) => {}
        other => panic!("an absent graph must refuse typed: {other:?}"),
    }
}

#[test]
fn attach_discovery_refuses_ambiguity_and_never_moves_existing_state() {
    let (_dir, clone) = cloned_co_tenant();
    // A second graph on the origin side, fetched into the clone.
    {
        let origin = clone.parent().unwrap().join("origin");
        let repo =
            Repository::init_co_tenant(&origin, "h", InitOptions::default()).expect("init h");
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(&node(2), &NodeRecord::new([], Default::default()))
            .expect("put");
        tx.commit("add 2", &[], None).expect("commit");
    }
    git(&clone, &["fetch", "origin"]);

    // Two candidates: discovery refuses, naming both.
    match Repository::attach_co_tenant(&clone, None) {
        Err(GraphError::AmbiguousAttach { candidates }) => {
            assert_eq!(candidates, vec!["g".to_owned(), "h".to_owned()]);
        }
        other => panic!("two candidates must refuse: {other:?}"),
    }

    // Explicit names attach each; g's re-attach after moving its HEAD to a
    // new branch must not reset it.
    Repository::attach_co_tenant(&clone, Some("g")).expect("attach g");
    let repo = Repository::open_graph(&clone, "g").expect("open g");
    repo.create_branch("feature", None).expect("branch");
    repo.checkout_branch("feature").expect("checkout");
    drop(repo);
    let outcome = Repository::attach_co_tenant(&clone, Some("g")).expect("re-attach g");
    assert!(
        !outcome.head_set,
        "re-attach must not reset HEAD: {outcome:?}"
    );
    let repo = Repository::open_graph(&clone, "g").expect("open g again");
    assert_eq!(
        repo.current_branch().expect("branch").as_deref(),
        Some("refs/heads/acetone/g/feature"),
        "the user's checkout survives re-attach"
    );
    drop(repo);

    let h = Repository::attach_co_tenant(&clone, Some("h")).expect("attach h");
    assert_eq!(h.graph, "h");
    let repo = Repository::open_graph(&clone, "h").expect("open h");
    assert!(
        repo.workspace_snapshot()
            .expect("snapshot")
            .get_node(&node(2))
            .expect("get")
            .is_some()
    );
}
