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
        Err(GraphError::NoAttachableGraph { .. }) => {}
        other => panic!("nothing to attach must refuse typed: {other:?}"),
    }
    // Naming a graph that is not on the remote refuses too.
    match Repository::attach_co_tenant(&clone, Some("g")) {
        Err(GraphError::NoAttachableGraph { .. }) => {}
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

/// PR #285 review F1: attach must refuse to layer a graph onto a
/// STANDALONE acetone repository — the legacy workspace fallback would
/// read the standalone workspace as the attached graph's own,
/// cross-wiring two graphs' data.
#[test]
fn attach_refuses_a_standalone_acetone_repository() {
    let (dir, _clone) = cloned_co_tenant();
    let origin = dir.path().join("origin");

    // A standalone acetone repo with staged (uncommitted) work.
    let standalone = dir.path().join("standalone");
    let repo = Repository::init(&standalone, InitOptions::default()).expect("init standalone");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(9), &NodeRecord::new([], Default::default()))
        .expect("put");
    tx.save().expect("stage without commit");
    drop(repo);

    git(
        &standalone,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&standalone, &["fetch", "origin"]);

    match Repository::attach_co_tenant(&standalone, None) {
        Err(GraphError::ExistingAcetoneWorkspace) => {}
        other => panic!("attach into a standalone repo must refuse: {other:?}"),
    }
}

/// PR #285 review F2: attaching a SECOND graph must first migrate a sole
/// existing graph's workspace off the pre-split shared ref — otherwise
/// its uncommitted work is silently orphaned the moment the repo becomes
/// multi-graph.
#[test]
fn attach_migrates_a_sole_graphs_shared_workspace_first() {
    use acetone_store::RefStore;
    let (dir, clone) = cloned_co_tenant();

    // Attach g, stage a second node (uncommitted), then SIMULATE the
    // pre-split layout: move g's workspace onto the legacy shared ref.
    Repository::attach_co_tenant(&clone, Some("g")).expect("attach g");
    let repo = Repository::open_graph(&clone, "g").expect("open g");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(2), &NodeRecord::new([], Default::default()))
        .expect("put");
    tx.save().expect("stage");
    let store = repo.store();
    let per_graph = "refs/worktree/acetone/g/workspace";
    let shared = "refs/worktree/acetone/workspace";
    let value = store
        .read_ref(per_graph)
        .expect("read")
        .expect("workspace present");
    store.write_ref(shared, None, &value).expect("plant shared");
    store.delete_ref(per_graph).expect("drop per-graph");
    drop(repo);

    // A second graph appears on the origin (with a commit, so it has a
    // branch to fetch) and is fetched.
    let origin = dir.path().join("origin");
    {
        let repo =
            Repository::init_co_tenant(&origin, "h", InitOptions::default()).expect("init h");
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(&node(7), &NodeRecord::new([], Default::default()))
            .expect("put");
        tx.commit("seed h", &[], None).expect("commit");
    }
    git(&clone, &["fetch", "origin"]);

    // Attach h: g's shared-ref workspace must be migrated, not orphaned.
    Repository::attach_co_tenant(&clone, Some("h")).expect("attach h");
    let repo = Repository::open_graph(&clone, "g").expect("open g");
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    assert!(
        snapshot.get_node(&node(2)).expect("get").is_some(),
        "g's staged work survives the second attach"
    );
    assert!(repo.is_dirty().expect("dirty"), "g is still dirty");
}

/// A graph lacking the default branch on the chosen remote refuses with a
/// reason naming it; and a marker-only partial state (a crash between
/// marker and branches) is healed by a re-run.
#[test]
fn attach_missing_main_refuses_and_partial_state_heals() {
    let (dir, clone) = cloned_co_tenant();
    let origin = dir.path().join("origin");

    // A graph on the remote with only a dev branch (ref plumbing —
    // attach copies refs, so any commit value serves).
    let head = git(&origin, &["rev-parse", "refs/heads/acetone/g/main"]);
    git(&origin, &["update-ref", "refs/heads/acetone/m/dev", &head]);
    git(&clone, &["fetch", "origin"]);
    match Repository::attach_co_tenant(&clone, Some("m")) {
        Err(GraphError::NoAttachableGraph { reason }) => {
            assert!(reason.contains("main"), "the reason names main: {reason}");
        }
        other => panic!("missing main must refuse: {other:?}"),
    }

    // Partial state: marker only (the crash window after the first write).
    let blob = git(&clone, &["hash-object", "-w", "-t", "blob", "/dev/null"]);
    git(&clone, &["update-ref", "refs/acetone/graphs/g", &blob]);
    let outcome = Repository::attach_co_tenant(&clone, Some("g")).expect("heal");
    assert!(
        !outcome.marker_written,
        "marker already present: {outcome:?}"
    );
    assert!(outcome.head_set, "the heal sets HEAD: {outcome:?}");
    assert_eq!(outcome.branches_created, vec!["main".to_owned()]);
}

/// Multi-remote semantics (PR #285 review F3): origin wins even over a
/// disagreeing remote (documented); non-origin remotes attach only in
/// byte-for-byte agreement; disagreement without an origin refuses.
#[test]
fn attach_multi_remote_precedence_and_refusal() {
    let (_dir, clone) = cloned_co_tenant();
    let origin_main = git(&clone, &["rev-parse", "refs/remotes/origin/acetone/g/main"]);
    let other = git(&clone, &["rev-parse", "HEAD"]); // any different hash

    // A disagreeing second remote: origin still wins, silently and
    // documentedly.
    git(
        &clone,
        &["update-ref", "refs/remotes/upstream/acetone/g/main", &other],
    );
    let outcome = Repository::attach_co_tenant(&clone, Some("g")).expect("attach");
    assert_eq!(outcome.remote.as_deref(), Some("origin"));
    let local = git(&clone, &["rev-parse", "refs/heads/acetone/g/main"]);
    assert_eq!(local, origin_main, "origin's value was attached");

    // Without origin: agreeing remotes attach; disagreeing ones refuse.
    git(
        &clone,
        &["update-ref", "-d", "refs/remotes/origin/acetone/g/main"],
    );
    git(
        &clone,
        &["update-ref", "refs/remotes/beta/acetone/g/main", &other],
    );
    // upstream and beta agree (both `other`): attach works (idempotent
    // no-op here since local state exists — outcome reports a remote).
    let outcome = Repository::attach_co_tenant(&clone, Some("g")).expect("agreeing remotes");
    assert!(outcome.remote.is_some());
    // Now they disagree.
    git(
        &clone,
        &[
            "update-ref",
            "refs/remotes/beta/acetone/g/main",
            &origin_main,
        ],
    );
    match Repository::attach_co_tenant(&clone, Some("g")) {
        Err(GraphError::DisagreeingRemotes { graph, remotes }) => {
            assert_eq!(graph, "g");
            assert_eq!(remotes, vec!["beta".to_owned(), "upstream".to_owned()]);
        }
        other => panic!("disagreeing non-origin remotes must refuse: {other:?}"),
    }
}

/// The standalone guard must see the workspace from ANY worktree
/// (acetone-zavr.9, PR #287 review F3): from a linked worktree of a
/// standalone repo, the per-worktree probe alone is blind to the main
/// worktree's workspace — both attach and init --co-tenant must still
/// refuse.
#[test]
fn the_standalone_guard_sees_across_worktrees() {
    let (dir, _clone) = cloned_co_tenant();
    let origin = dir.path().join("origin");

    // A standalone repo with uncommitted work in its MAIN worktree.
    let standalone = dir.path().join("standalone-wt");
    let repo = Repository::init(&standalone, InitOptions::default()).expect("init standalone");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(5), &NodeRecord::new([], Default::default()))
        .expect("put");
    tx.commit("base", &[], None).expect("commit");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(6), &NodeRecord::new([], Default::default()))
        .expect("put");
    tx.save().expect("stage");
    drop(repo);

    // A linked worktree; the co-tenant remote fetched from THERE.
    let wt = dir.path().join("standalone-linked");
    git(
        &standalone,
        &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
    );
    git(&wt, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&wt, &["fetch", "origin"]);

    match Repository::attach_co_tenant(&wt, None) {
        Err(GraphError::ExistingAcetoneWorkspace) => {}
        other => panic!("attach from a linked worktree must still refuse: {other:?}"),
    }
    match Repository::init_co_tenant(&wt, "layered", InitOptions::default()) {
        Err(GraphError::ExistingAcetoneWorkspace) => {}
        other => panic!("init --co-tenant from a linked worktree must refuse: {other:?}"),
    }
}
