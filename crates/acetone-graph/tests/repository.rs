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

/// Guard: the shipped chunk profile is exactly `(1024, 12, 65536)`. The prolly
/// golden suite (`acetone-prolly/tests/golden.rs`) hard-codes these same values
/// as `shipped_chunk_params()` to pin the on-disk format a real repository
/// produces (ADR-0045, acetone-7bn.18) — but cannot import this crate. This
/// test keeps the two in lock-step: changing the shipped profile trips here and
/// forces a deliberate golden re-pin (a format_version decision), rather than
/// silently drifting the goldens away from what `acetone init` writes.
#[test]
fn shipped_chunk_profile_is_pinned() {
    let p = acetone_graph::repo::default_chunk_params();
    assert_eq!(
        (p.min_bytes(), p.mask_bits(), p.max_bytes()),
        (1024, 12, 65536),
        "shipped chunk profile changed — re-pin acetone-prolly's golden suite \
         (shipped_chunk_params) with Gate-D care before updating this guard"
    );
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
fn a_detached_head_worktree_bootstraps_instead_of_failing() {
    // acetone-cm9: a fresh linked worktree checked out at a *detached* HEAD used
    // to fail every operation with a spurious "no acetone workspace" error,
    // because bootstrap resolved only a branch tip (None for a detached HEAD).
    // It must now bootstrap the worktree's workspace from the checked-out
    // commit.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "web1"), &record(&[("cores", 8)]))
        .expect("put");
    let commit = tx.commit("base", &[], None).expect("commit");

    let git_dir = repo.store().git_dir().to_path_buf();
    let wt = dir.path().join("wt-detached");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&git_dir)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(commit.to_hex())
        .status()
        .expect("run git worktree add");
    assert!(status.success(), "git worktree add --detach failed");

    // Opening the detached worktree must succeed and read-only ops must work.
    let wt_repo = Repository::open(&wt).expect("open detached worktree");
    assert!(
        wt_repo.current_branch().expect("branch").is_none(),
        "the worktree HEAD is detached"
    );
    let snap = wt_repo.workspace_snapshot().expect("snapshot");
    assert!(
        snap.get_node(&node("Host", "web1")).expect("get").is_some(),
        "the bootstrapped workspace sees the checked-out commit's data"
    );
    // A pristine detached bootstrap reads as clean, not spuriously dirty
    // (cm9 review, finding 1): is_dirty compares against the checked-out commit.
    assert!(
        !wt_repo.is_dirty().expect("dirty"),
        "a pristine detached bootstrap must be clean"
    );

    // Committing from a detached worktree fails cleanly (no branch to advance)
    // and — the branch check runs before staged writes are applied — leaves the
    // workspace untouched (cm9 review, finding 2).
    let mut wt_tx = wt_repo.begin_write().expect("begin");
    wt_tx
        .put_node(&node("Host", "web2"), &record(&[("cores", 4)]))
        .expect("stage");
    let err = wt_tx
        .commit("nope", &[], None)
        .expect_err("a detached commit must fail");
    assert!(matches!(err, GraphError::NoCurrentBranch), "got {err:?}");
    assert!(
        wt_repo
            .workspace_snapshot()
            .expect("snapshot")
            .get_node(&node("Host", "web2"))
            .expect("get")
            .is_none(),
        "a failed detached commit must not partially apply"
    );
}

#[test]
fn a_detached_worktree_bootstraps_from_its_own_commit_not_the_branch_tip() {
    // Bootstrap must follow the *detached* commit, not the branch tip: a
    // worktree detached at an earlier commit sees that commit's graph, not the
    // newer data on the branch (acetone-cm9).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "old"), &record(&[]))
        .expect("put");
    let earlier = tx.commit("c1", &[], None).expect("commit");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "new"), &record(&[]))
        .expect("put");
    tx.commit("c2", &[], None).expect("commit"); // branch tip now has both

    let git_dir = repo.store().git_dir().to_path_buf();
    let wt = dir.path().join("wt-earlier");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&git_dir)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(earlier.to_hex())
        .status()
        .expect("run git worktree add");
    assert!(status.success());

    let wt_repo = Repository::open(&wt).expect("open");
    let snap = wt_repo.workspace_snapshot().expect("snapshot");
    assert!(
        snap.get_node(&node("Host", "old")).expect("get").is_some(),
        "sees the detached commit's node"
    );
    assert!(
        snap.get_node(&node("Host", "new")).expect("get").is_none(),
        "must NOT see the branch tip's later node"
    );
}

#[test]
fn merge_from_detached_head_reports_no_current_branch_not_dirty() {
    // acetone-060: with a detached HEAD and a workspace that differs from the
    // checked-out commit, merge() must report the *accurate* precondition
    // failure — NoCurrentBranch (there is no branch to advance) — not the
    // incidental DirtyWorkspace. The on-a-branch check runs before the dirty
    // check. (Reachable once co-tenant mode drives its own head pointer, Phase
    // 8; constructed here directly.)
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    // Two commits on main; the workspace ends at c2.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "a"), &record(&[])).expect("put");
    let c1 = tx.commit("c1", &[], None).expect("commit");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "b"), &record(&[])).expect("put");
    tx.commit("c2", &[], None).expect("commit");

    // Detach HEAD at the earlier commit, leaving acetone's workspace ref at c2.
    // `git update-ref --no-deref` rewrites only HEAD, so the reopened repo is
    // BOTH detached (current_branch None) AND dirty (workspace c2 ≠ head c1).
    let git_dir = repo.store().git_dir().to_path_buf();
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&git_dir)
        .args(["update-ref", "--no-deref", "HEAD"])
        .arg(c1.to_hex())
        .status()
        .expect("run git update-ref");
    assert!(status.success(), "git update-ref --no-deref HEAD failed");

    let repo = Repository::open(&dir.path().join("graph.git")).expect("reopen");
    assert!(
        repo.current_branch().expect("branch").is_none(),
        "HEAD is detached"
    );
    assert!(
        repo.is_dirty().expect("dirty"),
        "workspace differs from the detached commit"
    );

    let err = repo.merge("main", "merge").expect_err("merge must fail");
    assert!(
        matches!(err, GraphError::NoCurrentBranch),
        "detached HEAD must report NoCurrentBranch, not DirtyWorkspace; got {err:?}"
    );
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
    // Commit with no staged changes on an unborn branch is refused by
    // default (acetone-k78) — an empty root commit is the explicit opt-in.
    match tx.commit("empty root", &[], None) {
        Err(GraphError::NothingToCommit) => {}
        other => panic!("expected NothingToCommit, got {other:?}"),
    }
    let tx = repo.begin_write().expect("begin");
    let root = tx
        .commit_allow_empty("empty root", &[], None)
        .expect("commit");
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
fn uncommitted_workspace_survives_git_gc() {
    // huo: a saved-but-uncommitted workspace anchors its chunk set in a
    // workspace tree, so even an aggressive foreign `git gc --prune=now`
    // keeps every chunk — no `commit before gc` caveat.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo_path = dir.path().join("graph.git");
    let repo = Repository::init(&repo_path, InitOptions::default()).expect("init");

    // Multi-chunk maps, saved to the workspace but NEVER committed.
    let mut tx = repo.begin_write().expect("begin");
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
    tx.save().expect("save"); // save, not commit — history is empty
    assert!(repo.head_commit().expect("head").is_none());

    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo_path)
        .args(["gc", "--prune=now", "--aggressive", "--quiet"])
        .status()
        .expect("run git gc");
    assert!(status.success(), "git gc failed");

    // Reopen cold: the uncommitted workspace reads back in full.
    let repo = Repository::open(&repo_path).expect("open");
    let snapshot = repo.workspace_snapshot().expect("workspace survives gc");
    assert_eq!(snapshot.nodes().expect("nodes").len(), 500);
    assert_eq!(snapshot.edges().expect("edges").len(), 250);
    assert_eq!(snapshot.reverse_edge_keys().expect("rev").len(), 250);
}

#[test]
fn a_linked_worktrees_uncommitted_workspace_survives_git_gc() {
    // acetone-7tf (fixed by ADR-0044): the huo durability guarantee (ADR-0015)
    // now holds for a *linked* worktree too, not only the main one. A linked
    // worktree's workspace ref is a worktree-private ref
    // (`<common>/worktrees/<id>/refs/worktree/acetone/workspace`), and git's gc
    // reachability walk does NOT enumerate another worktree's `refs/worktree/*`
    // refs as roots (confirmed pure-git, 2.48.1) — so on its own the workspace
    // would be pruned by a foreign `git gc` from the main worktree. cas_workspace
    // therefore also force-updates a COMMON anchor ref
    // `refs/acetone/worktree-anchors/<id>` -> the workspace tree; because that
    // ref lives in the common ref store, git enumerates it globally as a gc root.
    // This test proves end-to-end that gix writes `refs/acetone/*` to the common
    // dir (not per-worktree) and that the linked worktree's saved-but-uncommitted
    // chunks survive an aggressive foreign `git gc --prune=now` from main.
    let dir = tempfile::tempdir().expect("tempdir");
    let main_git = dir.path().join("graph.git");
    let repo = Repository::init(&main_git, InitOptions::default()).expect("init");

    // A commit in the main worktree, so `git worktree add` has a committish.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "seed"), &record(&[]))
        .expect("seed");
    let base = tx.commit("seed", &[], None).expect("commit");

    // A linked worktree checked out at the seed commit (detached).
    let wt = dir.path().join("wt-linked");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_git)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(base.to_hex())
        .status()
        .expect("run git worktree add");
    assert!(status.success(), "git worktree add failed");

    // In the linked worktree, save (never commit) a multi-chunk batch of work.
    let wt_repo = Repository::open(&wt).expect("open linked worktree");
    let mut tx = wt_repo.begin_write().expect("begin");
    for i in 0..500 {
        tx.put_node(
            &node("Host", &format!("host-{i:04}")),
            &record(&[("index", i)]),
        )
        .expect("node");
    }
    tx.save().expect("save"); // save, not commit — the linked worktree is detached
    assert!(
        wt_repo.is_dirty().expect("dirty"),
        "uncommitted work is present"
    );

    // The anchor ref must live in the COMMON ref store (not the worktree-private
    // one) — that is the whole reason git enumerates it as a gc root. The linked
    // worktree's private refs live under `<common>/worktrees/<id>/`, so finding
    // the anchor directly under `<main_git>/refs/acetone/worktree-anchors/`
    // confirms gix routed it to the common dir.
    let anchors_dir = main_git.join("refs/acetone/worktree-anchors");
    let anchors: Vec<_> = std::fs::read_dir(&anchors_dir)
        .expect("anchors dir exists in common ref store")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(
        anchors.len(),
        1,
        "exactly one linked-worktree anchor in the common ref store, got {anchors:?}"
    );

    // Aggressive foreign gc from the MAIN worktree.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_git)
        .args(["gc", "--prune=now", "--aggressive", "--quiet"])
        .status()
        .expect("run git gc");
    assert!(status.success(), "git gc failed");

    // Reopen the linked worktree cold: its uncommitted work reads back in full —
    // the private-ref-anchored chunks survived the prune.
    let wt_repo = Repository::open(&wt).expect("reopen linked worktree");
    let snapshot = wt_repo.workspace_snapshot().expect("workspace survives gc");
    // The 500 saved nodes plus the seed the worktree bootstrapped from.
    assert_eq!(snapshot.nodes().expect("nodes").len(), 501);
    assert!(
        snapshot
            .get_node(&node("Host", "host-0499"))
            .expect("get")
            .is_some(),
        "a chunk-anchored uncommitted node survived gc"
    );
}

#[test]
fn acetone_gc_prunes_a_removed_worktrees_stale_anchor() {
    // acetone-7tf (ADR-0044): the common anchor ref keeps a linked worktree's
    // uncommitted chunks alive. Once that worktree is removed it must NOT keep
    // pinning them — acetone's own gc runs only when no linked worktree exists
    // (ADR-0014), so every surviving anchor is stale and gc deletes them all,
    // letting consolidation reclaim their chunks.
    let dir = tempfile::tempdir().expect("tempdir");
    let main_git = dir.path().join("graph.git");
    let repo = Repository::init(&main_git, InitOptions::default()).expect("init");

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("Host", "seed"), &record(&[]))
        .expect("seed");
    let base = tx.commit("seed", &[], None).expect("commit");

    // A linked worktree that saves uncommitted work, creating an anchor.
    let wt = dir.path().join("wt-linked");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_git)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(base.to_hex())
        .status()
        .expect("run git worktree add");
    assert!(status.success(), "git worktree add failed");

    let wt_repo = Repository::open(&wt).expect("open linked worktree");
    let mut tx = wt_repo.begin_write().expect("begin");
    for i in 0..500 {
        tx.put_node(
            &node("Host", &format!("host-{i:04}")),
            &record(&[("index", i)]),
        )
        .expect("node");
    }
    tx.save().expect("save");

    let anchors_dir = main_git.join("refs/acetone/worktree-anchors");
    assert!(anchors_dir.exists(), "an anchor was created");

    // Remove the worktree and prune git's record of it, so no linked worktree
    // remains and acetone gc will run.
    std::fs::remove_dir_all(&wt).expect("remove worktree dir");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_git)
        .args(["worktree", "prune"])
        .status()
        .expect("run git worktree prune");
    assert!(status.success(), "git worktree prune failed");

    // acetone gc now runs (no linked worktrees) and must delete the stale anchor.
    repo.gc().expect("gc runs with no linked worktrees");
    let remaining: Vec<_> = std::fs::read_dir(&anchors_dir)
        .map(|it| it.map(|e| e.expect("entry").file_name()).collect())
        .unwrap_or_default();
    assert!(
        remaining.is_empty(),
        "gc pruned every stale worktree anchor, remaining: {remaining:?}"
    );
}

#[test]
fn diff_classifies_node_and_edge_changes() {
    use acetone_graph::diff::ChangeKind;
    use acetone_model::Value;
    use acetone_model::records::EdgeRecord;
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let (a, b, c, d) = (
        node("Host", "a"),
        node("Host", "b"),
        node("Host", "c"),
        node("Host", "d"),
    );
    let weight = |w: i64| EdgeRecord::new(BTreeMap::from([("weight".to_string(), Value::Int(w))]));

    // v1: a{cores:8}, b, c ; a-RUNS->b {weight:1}, a-RUNS->c
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&a, &record(&[("cores", 8)])).expect("a");
    tx.put_node(&b, &record(&[])).expect("b");
    tx.put_node(&c, &record(&[])).expect("c");
    tx.put_edge(&edge(&a, "RUNS", &b), &weight(1)).expect("ab");
    tx.put_edge(&edge(&a, "RUNS", &c), &EdgeRecord::default())
        .expect("ac");
    let v1 = tx.commit("v1", &[], None).expect("commit v1");

    // v2: modify a{cores:16}, remove c, add d ; modify a-RUNS->b {weight:2},
    //     remove a-RUNS->c, add a-RUNS->d ; b left untouched.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&a, &record(&[("cores", 16)])).expect("a'");
    tx.delete_node(&c).expect("del c");
    tx.put_node(&d, &record(&[])).expect("d");
    tx.put_edge(&edge(&a, "RUNS", &b), &weight(2)).expect("ab'");
    tx.delete_edge(&edge(&a, "RUNS", &c)).expect("del ac");
    tx.put_edge(&edge(&a, "RUNS", &d), &EdgeRecord::default())
        .expect("ad");
    let v2 = tx.commit("v2", &[], None).expect("commit v2");

    let diff = repo.diff(&v1.to_hex(), &v2.to_hex()).expect("diff");

    // Nodes, in key order: a Modified, c Removed, d Added (b unchanged, so
    // it is absent from the diff).
    let node_kinds: Vec<_> = diff.nodes.iter().map(|n| (n.key.clone(), n.kind)).collect();
    assert_eq!(
        node_kinds,
        vec![
            (a.clone(), ChangeKind::Modified),
            (c.clone(), ChangeKind::Removed),
            (d.clone(), ChangeKind::Added),
        ]
    );
    let a_change = diff.nodes.iter().find(|n| n.key == a).expect("a change");
    assert_eq!(a_change.before, Some(record(&[("cores", 8)])));
    assert_eq!(a_change.after, Some(record(&[("cores", 16)])));

    // Edges, in key order (dst b<c<d): modified, removed, added.
    let edge_kinds: Vec<_> = diff.edges.iter().map(|e| e.kind).collect();
    assert_eq!(
        edge_kinds,
        vec![ChangeKind::Modified, ChangeKind::Removed, ChangeKind::Added,]
    );

    // A version differs from itself in nothing.
    assert!(
        repo.diff(&v2.to_hex(), &v2.to_hex())
            .expect("self-diff")
            .is_empty()
    );
}

#[test]
fn rekey_moves_a_node_and_its_edges_in_one_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    let old = node("Host", "old-01");
    let sw = node("Software", "nginx");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&old, &record(&[("cores", 8)])).expect("node");
    tx.put_node(&sw, &record(&[])).expect("node");
    tx.put_edge(&edge(&old, "RUNS", &sw), &EdgeRecord::default())
        .expect("edge");
    tx.commit("seed", &[], None).expect("commit");

    let new = node("Host", "web-01");
    let commit = repo.rekey(&old, &new, "rename host").expect("rekey");
    assert_eq!(repo.head_commit().expect("head"), Some(commit));

    let snapshot = repo.workspace_snapshot().expect("snapshot");
    // Old identity gone, new identity present with the same record.
    assert!(snapshot.get_node(&old).expect("get").is_none());
    assert_eq!(
        snapshot.get_node(&new).expect("get"),
        Some(record(&[("cores", 8)]))
    );
    // The edge was rewritten onto the new key (and edges_rev with it).
    let edges = snapshot.edges().expect("edges");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].0.src(), &new);
    assert_eq!(snapshot.reverse_edge_keys().expect("rev").len(), 1);

    // fsck stays clean (no dangling edge, edges_rev consistent).
    let report = acetone_graph::fsck::check(&repo).expect("fsck");
    assert!(report.is_clean(), "fsck: {report:?}");

    // Rekey to an existing key, to the same key, or of an absent node all
    // error cleanly.
    let other = node("Host", "other");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&other, &record(&[])).expect("node");
    tx.save().expect("save");
    assert!(matches!(
        repo.rekey(&new, &other, "clash"),
        Err(GraphError::RekeyConflict { .. })
    ));
    assert!(matches!(
        repo.rekey(&new, &new, "self"),
        Err(GraphError::RekeyConflict { .. })
    ));
    assert!(matches!(
        repo.rekey(&node("Host", "ghost"), &node("Host", "z"), "absent"),
        Err(GraphError::NoSuchNode { .. })
    ));
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

/// Declare label `name` with the given key tuple.
fn declare_label_key(tx: &mut acetone_graph::repo::Transaction<'_>, name: &str, key: &[&str]) {
    tx.put_schema(&SchemaEntry::Label {
        name: name.into(),
        def: LabelDef::new(
            key.iter().map(|s| (*s).to_owned()).collect(),
            BTreeMap::new(),
            [],
            [],
        )
        .expect("valid label def"),
    })
    .expect("put schema");
}

#[test]
fn changing_a_label_key_over_live_data_is_rejected() {
    // U3 (pre-0.1 review): node identity is (primary label, key tuple), so
    // changing a label's key while nodes exist under the old key would orphan
    // their identity — must be rejected (Invariant #3).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Host", &["name"]);
    tx.put_node(&node("Host", "web1"), &record(&[]))
        .expect("node");
    tx.commit("seed", &[], None).expect("commit");

    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Host", &["id"]); // different key tuple
    match tx.save() {
        Err(GraphError::LabelKeyChanged { label }) => assert_eq!(label, "Host"),
        other => panic!("expected LabelKeyChanged, got {other:?}"),
    }
}

#[test]
fn redeclaring_a_label_with_the_same_key_over_live_data_is_allowed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Host", &["name"]);
    tx.put_node(&node("Host", "web1"), &record(&[]))
        .expect("node");
    tx.commit("seed", &[], None).expect("commit");

    // Re-declaring the identical key tuple is a no-op change, not a corruption.
    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Host", &["name"]);
    tx.save().expect("same-key redeclare must be allowed");
}

#[test]
fn changing_a_label_key_before_any_data_is_allowed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Host", &["name"]);
    tx.commit("declare", &[], None).expect("commit");

    // No nodes yet: refining the key before adding data is fine.
    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Host", &["id"]);
    tx.save().expect("key change before data must be allowed");
}

#[test]
fn reordering_a_composite_label_key_over_live_data_is_rejected() {
    // A composite key's property order is significant (it is the key tuple), so
    // reordering it over live data is a rejected identity change.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Reading", &["sensor", "at"]);
    let key = NodeKey::new(
        "Reading",
        vec![Value::String("s1".into()), Value::String("t1".into())],
    )
    .expect("key");
    tx.put_node(&key, &record(&[])).expect("node");
    tx.commit("seed", &[], None).expect("commit");

    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Reading", &["at", "sensor"]); // reordered
    match tx.save() {
        Err(GraphError::LabelKeyChanged { label }) => assert_eq!(label, "Reading"),
        other => panic!("expected LabelKeyChanged, got {other:?}"),
    }
}

#[test]
fn changing_a_key_of_a_label_used_only_as_secondary_is_allowed() {
    // The key tuple belongs to a node's PRIMARY label. A node bearing L only as
    // a secondary label is keyed by its own primary label, so changing L's key
    // cannot orphan it — the change is allowed.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Host", &["name"]);
    declare_label_key(&mut tx, "Tagged", &["x"]);
    // A node whose PRIMARY label is Host, bearing Tagged as a secondary label.
    let key = NodeKey::new("Host", vec![Value::String("web1".into())]).expect("key");
    let rec = NodeRecord::new(["Tagged".to_string()], BTreeMap::new());
    tx.put_node(&key, &rec).expect("node");
    tx.commit("seed", &[], None).expect("commit");

    // Changing Tagged's key is fine: no node has Tagged as its primary label.
    let mut tx = repo.begin_write().expect("begin");
    declare_label_key(&mut tx, "Tagged", &["y"]);
    tx.save()
        .expect("changing a secondary-only label's key must be allowed");
}
