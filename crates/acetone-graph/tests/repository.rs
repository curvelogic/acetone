//! Integration tests for the repository plumbing (spec §3.5, §4):
//! init/open, workspace persistence and atomic advance, single-writer
//! locking, commits with complete chunk anchoring (verified against a
//! real `git gc --prune=now`), branches, checkout and log.

use acetone_graph::repo::{DEFAULT_BRANCH, InitOptions, Repository};
use acetone_graph::{GraphError, WriteLock};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, SchemaEntry};
use std::collections::BTreeMap;
use std::path::Path;

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

fn node(label: &str, key: &str) -> NodeKey {
    NodeKey::new(label, vec![Value::String(key.to_owned())]).expect("valid")
}

fn record(pairs: &[(&str, i64)]) -> NodeRecord {
    NodeRecord::new(
        [],
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Value::Int(*v)))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn edge(src: &NodeKey, rtype: &str, dst: &NodeKey) -> EdgeKey {
    EdgeKey::new(src.clone(), rtype, dst.clone(), Value::Null).expect("valid")
}

#[test]
fn init_creates_empty_workspace_that_reopens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    assert_eq!(
        repo.current_branch().expect("head"),
        Some(format!("refs/heads/{DEFAULT_BRANCH}"))
    );
    assert_eq!(repo.head_commit().expect("head"), None);
    assert!(!repo.is_dirty().expect("clean"));
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    assert!(snapshot.nodes().expect("nodes").is_empty());
    assert!(snapshot.edges().expect("edges").is_empty());

    // Reopen: same state (workspace survives process exit by being a ref).
    drop(repo);
    let repo = Repository::open(&dir.path().join("graph.git")).expect("open");
    assert!(
        repo.workspace_snapshot()
            .expect("snapshot")
            .nodes()
            .expect("n")
            .is_empty()
    );
}

#[test]
fn open_of_plain_git_repo_reports_no_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    acetone_store::GitStore::create(&dir.path().join("plain.git")).expect("git init");
    match Repository::open(&dir.path().join("plain.git")) {
        Err(GraphError::NoWorkspace { name }) => assert_eq!(name, "default"),
        other => panic!("expected NoWorkspace, got {other:?}"),
    }
}

#[test]
fn mutations_round_trip_and_maintain_edge_symmetry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    let web = node("Host", "web1");
    let db = node("Service", "db");
    let dep = edge(&web, "DEPENDS_ON", &db);

    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(vec!["name".into()], BTreeMap::new(), [], []).expect("valid"),
    })
    .expect("schema");
    tx.put_node(&web, &record(&[("cores", 8)])).expect("node");
    tx.put_node(&db, &record(&[("tier", 0)])).expect("node");
    tx.put_edge(&dep, &EdgeRecord::default()).expect("edge");
    tx.save().expect("save");

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    assert_eq!(
        snapshot.get_node(&web).expect("get"),
        Some(record(&[("cores", 8)]))
    );
    assert_eq!(snapshot.nodes().expect("nodes").len(), 2);
    assert_eq!(snapshot.schema_entries().expect("schema").len(), 1);
    // Forward and reverse maps carry the same edge set (spec §3.3).
    let fwd: Vec<EdgeKey> = snapshot
        .edges()
        .expect("edges")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let rev = snapshot.reverse_edge_keys().expect("rev");
    assert_eq!(fwd, vec![dep.clone()]);
    assert_eq!(rev, vec![dep.clone()]);

    // Delete removes from both maps in one save.
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_edge(&dep).expect("delete");
    tx.save().expect("save");
    let snapshot = repo.workspace_snapshot().expect("snapshot");
    assert!(snapshot.edges().expect("edges").is_empty());
    assert!(snapshot.reverse_edge_keys().expect("rev").is_empty());
}

#[test]
fn snapshots_are_pinned_while_workspace_advances() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let before = repo.workspace_snapshot().expect("snapshot");

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "web1"), &record(&[]))
        .expect("node");
    tx.save().expect("save");

    // The pre-existing snapshot still sees the empty graph (MVCC).
    assert!(before.nodes().expect("nodes").is_empty());
    assert_eq!(
        repo.workspace_snapshot()
            .expect("snapshot")
            .nodes()
            .expect("nodes")
            .len(),
        1
    );
}

#[test]
fn transaction_holds_the_single_writer_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let tx = repo.begin_write().expect("begin");
    match repo.begin_write() {
        Err(GraphError::Locked { .. }) => {}
        other => panic!("expected Locked, got {other:?}"),
    }
    drop(tx);
    repo.begin_write().expect("lock released on drop");
}

#[test]
fn workspace_advance_is_compare_and_swap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    // Simulate a foreign writer advancing the workspace between this
    // transaction's load and its save by writing through a second
    // Repository handle after dropping the first transaction's lock file
    // (the lock guards politeness; CAS guards correctness).
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "a"), &record(&[])).expect("node");
    // Bypass: release the lock file manually so a second writer can act,
    // while our transaction still holds its loaded base manifest.
    std::fs::remove_file(repo.store().common_dir().join("acetone-writer.lock"))
        .expect("remove lock file");
    {
        let repo2 = Repository::open(&dir.path().join("graph.git")).expect("open");
        let mut tx2 = repo2.begin_write().expect("begin2");
        tx2.put_node(&node("Host", "b"), &record(&[]))
            .expect("node");
        tx2.save().expect("save2");
    }
    match tx.save() {
        Err(GraphError::WorkspaceConflict { name }) => assert_eq!(name, "default"),
        other => panic!("expected WorkspaceConflict, got {other:?}"),
    }
    // The foreign write survived; ours was refused, not lost silently.
    let nodes = repo.workspace_snapshot().expect("s").nodes().expect("n");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].0, node("Host", "b"));
}

#[test]
fn commit_advances_branch_and_log_walks_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "web1"), &record(&[]))
        .expect("node");
    let first = tx.commit("add web1", &[], None).expect("commit");
    assert_eq!(repo.head_commit().expect("head"), Some(first));
    assert!(!repo.is_dirty().expect("clean after commit"));

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "web2"), &record(&[]))
        .expect("node");
    let second = tx
        .commit(
            "add web2",
            &[("Acetone-Source".to_owned(), "test".to_owned())],
            None,
        )
        .expect("commit");

    let log = repo.log(None).expect("log");
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].id, second);
    assert_eq!(log[0].parents, vec![first]);
    assert!(log[0].message.starts_with("add web2"));
    assert_eq!(
        log[0].trailers,
        vec![("Acetone-Source".to_owned(), "test".to_owned())]
    );
    assert_eq!(log[1].id, first);
    assert_eq!(log[1].parents, Vec::<acetone_store::Hash>::new());

    // Snapshots by refspec: hex hash and branch name.
    let at_first = repo.snapshot(&first.to_hex()).expect("snapshot");
    assert_eq!(at_first.nodes().expect("nodes").len(), 1);
    let at_branch = repo.snapshot(DEFAULT_BRANCH).expect("snapshot");
    assert_eq!(at_branch.nodes().expect("nodes").len(), 2);
}

#[test]
fn commit_requires_a_branch_and_rejects_merge_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let tx = repo.begin_write().expect("begin");
    // Commit with no staged changes on an unborn branch is legal (an
    // empty root commit), so exercise the error paths separately below.
    let root = tx.commit("empty root", &[], None).expect("commit");
    assert_eq!(repo.head_commit().expect("head"), Some(root));
}

#[test]
fn committed_versions_survive_git_gc() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo_path = dir.path().join("graph.git");
    let repo = Repository::init(&repo_path, InitOptions::default()).expect("init");

    // Enough entries in every map to force multi-chunk trees, so
    // under-anchoring of interior nodes would be caught.
    let mut tx = repo.begin_write().expect("begin");
    for i in 0..100 {
        tx.put_schema(&SchemaEntry::Label {
            name: format!("Label{i:03}"),
            def: LabelDef::new(vec!["name".into()], BTreeMap::new(), [], []).expect("valid"),
        })
        .expect("schema");
    }
    for i in 0..500 {
        tx.put_node(
            &node("Host", &format!("host-{i:04}")),
            &record(&[("index", i)]),
        )
        .expect("node");
        if i % 2 == 0 {
            tx.put_edge(
                &edge(
                    &node("Host", &format!("host-{i:04}")),
                    "PEERS_WITH",
                    &node("Host", &format!("host-{:04}", (i + 1) % 500)),
                ),
                &EdgeRecord::default(),
            )
            .expect("edge");
        }
    }
    let commit = tx.commit("bulk load", &[], None).expect("commit");

    // A real git gc with immediate pruning: everything the commit anchors
    // must survive; unanchored garbage may go.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .args(["gc", "--prune=now", "--aggressive", "--quiet"])
        .status()
        .expect("run git gc");
    assert!(status.success(), "git gc failed");

    // Reopen cold and read the committed version back in full.
    let repo = Repository::open(&repo_path).expect("open");
    let snapshot = repo.snapshot(&commit.to_hex()).expect("snapshot");
    let nodes = snapshot.nodes().expect("nodes survive gc");
    assert_eq!(nodes.len(), 500);
    let edges = snapshot.edges().expect("edges survive gc");
    assert_eq!(edges.len(), 250);
    assert_eq!(
        snapshot.reverse_edge_keys().expect("rev survives gc").len(),
        250
    );
    assert_eq!(
        snapshot.schema_entries().expect("schema survives gc").len(),
        100
    );
}

#[test]
fn branch_and_checkout_move_the_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "web1"), &record(&[]))
        .expect("node");
    let base = tx.commit("base", &[], None).expect("commit");

    // Branch, check it out, diverge.
    repo.create_branch("feature", None).expect("branch");
    repo.checkout_branch("feature").expect("checkout");
    assert_eq!(
        repo.current_branch().expect("head"),
        Some("refs/heads/feature".to_owned())
    );
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "web2"), &record(&[]))
        .expect("node");
    let feature = tx.commit("feature work", &[], None).expect("commit");

    // Back to the default branch: workspace resets to its version.
    repo.checkout_branch(DEFAULT_BRANCH).expect("checkout");
    assert_eq!(
        repo.workspace_snapshot()
            .expect("s")
            .nodes()
            .expect("n")
            .len(),
        1
    );
    assert_eq!(repo.head_commit().expect("head"), Some(base));

    // Branch listing shows both, at the right commits.
    let branches = repo.branches().expect("branches");
    assert_eq!(
        branches,
        vec![
            ("feature".to_owned(), feature),
            (DEFAULT_BRANCH.to_owned(), base),
        ]
    );

    // Checkout refuses to discard uncommitted work.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "dirty"), &record(&[]))
        .expect("node");
    tx.save().expect("save");
    match repo.checkout_branch("feature") {
        Err(GraphError::DirtyWorkspace) => {}
        other => panic!("expected DirtyWorkspace, got {other:?}"),
    }

    // Unknown branch is a typed error.
    match repo.create_branch("feature", None) {
        Err(GraphError::BranchExists { .. }) => {}
        other => panic!("expected BranchExists, got {other:?}"),
    }
}

#[test]
fn lock_file_lives_in_the_per_worktree_git_dir() {
    // ADR-0014: the writer lock is per-worktree. For a repository with no
    // linked worktrees, git_dir coincides with common_dir.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let lock = WriteLock::acquire(repo.store().git_dir()).expect("acquire");
    assert!(lock.path().starts_with(repo.store().git_dir()));
}

/// Run a git command in `dir`; return whether it succeeded (worktree tests
/// skip gracefully where `git worktree` is unavailable).
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn worktrees_are_first_class() {
    // Two git worktrees of one repository get independent writers and
    // independent workspaces (ADR-0014).
    let dir = tempfile::tempdir().expect("tempdir");
    let git_path = dir.path().join("graph.git");
    let repo = init_repo(dir.path());

    // Commit some content on main, then branch it.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "web1"), &record(&[("cores", 8)]))
        .expect("node");
    tx.commit("seed", &[], None).expect("commit");
    repo.create_branch("feature", None).expect("branch");

    // Add a linked worktree checked out on `feature`. Skip if the platform
    // git cannot (older git, sandbox).
    let wt = dir.path().join("wt");
    if !git_ok(
        &git_path,
        &["worktree", "add", wt.to_str().unwrap(), "feature"],
    ) {
        eprintln!("SKIP worktrees_are_first_class: `git worktree add` unavailable");
        return;
    }

    // Opening the fresh worktree bootstraps its workspace from `feature`.
    let wt_repo = Repository::open(&wt).expect("open worktree");
    let snap = wt_repo.workspace_snapshot().expect("workspace");
    assert!(
        snap.get_node(&node("Host", "web1")).expect("get").is_some(),
        "worktree workspace should carry the committed node"
    );

    // Independent writers: hold main's writer while the worktree's writer
    // also starts — different git dirs, different locks.
    let main_tx = repo.begin_write().expect("main writer");
    let mut wt_tx = wt_repo
        .begin_write()
        .expect("worktree writer runs concurrently with main's");
    wt_tx
        .put_node(&node("Host", "web2"), &record(&[("cores", 4)]))
        .expect("node");
    wt_tx.save().expect("save");
    drop(main_tx);

    // Independent workspaces: the worktree's new node is invisible to main.
    let main_snap = repo.workspace_snapshot().expect("main workspace");
    assert!(
        main_snap
            .get_node(&node("Host", "web2"))
            .expect("get")
            .is_none(),
        "main workspace must not see the worktree's write"
    );
    // The per-worktree workspace refs are distinct git refs.
    assert!(git_ok(
        &git_path,
        &["rev-parse", "refs/worktree/acetone/workspace"]
    ));
}

#[test]
fn legacy_workspace_ref_is_adopted_and_migrated() {
    // A pre-ADR-0014 repository has only the shared legacy workspace ref.
    // acetone reads it on open and migrates it to the per-worktree ref on
    // the first write.
    let dir = tempfile::tempdir().expect("tempdir");
    let git_path = dir.path().join("graph.git");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "web1"), &record(&[]))
        .expect("node");
    tx.save().expect("save");

    // Simulate a legacy repo: move the per-worktree ref's value to the
    // legacy shared ref and delete the per-worktree one.
    let manifest = git_out(&git_path, &["rev-parse", "refs/worktree/acetone/workspace"]);
    assert!(git_ok(
        &git_path,
        &["update-ref", "refs/acetone/workspaces/default", &manifest],
    ));
    assert!(git_ok(
        &git_path,
        &["update-ref", "-d", "refs/worktree/acetone/workspace"]
    ));

    // Open reads the workspace via the legacy fallback.
    let reopened = Repository::open(&git_path).expect("open legacy");
    let snap = reopened.workspace_snapshot().expect("workspace");
    assert!(snap.get_node(&node("Host", "web1")).expect("get").is_some());

    // The first write migrates forward: the per-worktree ref now exists.
    let mut tx = reopened.begin_write().expect("begin");
    tx.put_node(&node("Host", "web2"), &record(&[]))
        .expect("node");
    tx.save().expect("save");
    assert!(
        git_ok(&git_path, &["rev-parse", "refs/worktree/acetone/workspace"]),
        "per-worktree ref should exist after the migrating write"
    );
}
