//! Reader isolation while a writer holds the lock (acetone-d5is.2; spec §4,
//! `docs/protocol.md` "Concurrency and isolation").
//!
//! The read path takes no lock: a reader pins one immutable manifest — the
//! workspace's, or a commit's — and reads chunks by address. These tests pin
//! that guarantee against every writer that holds the single-writer lock and
//! moves refs or objects underneath readers: an open write transaction, a
//! clean and a conflicted merge, `abort_merge`, and `gc` (which repacks and
//! prunes loose objects). Every read must succeed and must observe either
//! the state before the operation or the state after it, never a mixture.
//!
//! They also pin the three two-step ref windows, where an operation moves
//! two refs one after the other and a reader can observe the state between
//! them. Each window keeps its order deliberately (the crash-safe order: a
//! crash between the steps leaves a state that `commit` refuses or that the
//! next step heals), so the transient state is documented rather than
//! closed; each test reconstructs that state and asserts what a reader
//! observes in it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use acetone_graph::GraphError;
use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository, ResolveSide};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_store::{CommitStore, Hash, RefStore};

/// This graph's per-worktree merge-head ref (standalone layout).
const MERGE_HEAD: &str = "refs/worktree/acetone/merge-head";

fn node(id: i64) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(id)]).expect("valid key")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(v))]))
}

fn init(dir: &Path) -> (PathBuf, Repository) {
    let path = dir.join("g.git");
    let repo = Repository::init(&path, InitOptions::default()).expect("init");
    (path, repo)
}

/// A committed base of 200 nodes (enough for multi-chunk maps), forked:
/// `other` (theirs) and `main` (ours) each commit one edit. With
/// `conflicting`, both sides set node 0's `v` differently (a cell
/// conflict); otherwise each side edits its own node (a clean merge).
/// Returns (ours, theirs).
fn forked(repo: &Repository, conflicting: bool) -> (Hash, Hash) {
    let mut tx = repo.begin_write().expect("begin");
    for id in 0..200 {
        tx.put_node(&node(id), &record(0)).expect("put");
    }
    let base = tx.commit("base", &[], None).expect("commit base");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout other");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(if conflicting { 0 } else { 1 }), &record(2))
        .expect("put");
    let theirs = tx.commit("theirs", &[], None).expect("commit theirs");

    repo.checkout_branch("main").expect("checkout main");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(0), &record(1)).expect("put");
    let ours = tx.commit("ours", &[], None).expect("commit ours");
    (ours, theirs)
}

/// One reader observation: the manifest a read pinned, and the graph read
/// through it — reading every node exercises the chunk reads that `gc`
/// could disturb, not just the ref.
type Observation = (Vec<u8>, Vec<(NodeKey, NodeRecord)>);

fn observe_workspace(repo: &Repository) -> Observation {
    let snap = repo.workspace_snapshot().expect("workspace read");
    let nodes = snap.nodes().expect("workspace nodes");
    (snap.manifest().encode(), nodes)
}

fn observe_main(repo: &Repository) -> Observation {
    let snap = repo.snapshot("main").expect("read at main");
    let nodes = snap.nodes().expect("nodes at main");
    (snap.manifest().encode(), nodes)
}

/// Readers racing a writer: each opens its own handle (as another process
/// or daemon connection would) and reads the workspace and `main` in a
/// loop until told to stop. `start` returns once every reader has made at
/// least one observation, so the readers are live before the writer runs.
struct Readers {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<(Vec<Observation>, Vec<Observation>)>>,
}

impl Readers {
    fn start(path: &Path, count: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicUsize::new(0));
        let handles = (0..count)
            .map(|_| {
                let path = path.to_owned();
                let stop = Arc::clone(&stop);
                let ready = Arc::clone(&ready);
                std::thread::spawn(move || {
                    let repo = Repository::open(&path).expect("reader open");
                    let (mut ws, mut main) = (Vec::new(), Vec::new());
                    loop {
                        ws.push(observe_workspace(&repo));
                        main.push(observe_main(&repo));
                        if ws.len() == 1 {
                            ready.fetch_add(1, Ordering::SeqCst);
                        }
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    (ws, main)
                })
            })
            .collect();
        while ready.load(Ordering::SeqCst) < count {
            std::thread::yield_now();
        }
        Readers { stop, handles }
    }

    /// Stop the readers and return every observation (workspace, main).
    fn finish(self) -> (Vec<Observation>, Vec<Observation>) {
        self.stop.store(true, Ordering::SeqCst);
        let (mut ws, mut main) = (Vec::new(), Vec::new());
        for handle in self.handles {
            let (w, m) = handle.join().expect("a reader panicked: a read failed");
            ws.extend(w);
            main.extend(m);
        }
        (ws, main)
    }
}

fn assert_pre_or_post(what: &str, seen: &[Observation], pre: &Observation, post: &Observation) {
    for (i, obs) in seen.iter().enumerate() {
        assert!(
            obs == pre || obs == post,
            "{what} read #{i} observed neither the pre- nor the post-state"
        );
    }
}

#[test]
fn reads_proceed_and_see_the_pre_state_while_a_write_transaction_holds_the_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, repo) = init(dir.path());
    forked(&repo, false);
    let pre_ws = observe_workspace(&repo);
    let pre_main = observe_main(&repo);

    // An open transaction holds the single-writer lock with staged work.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(500), &record(5)).expect("stage");

    // Another handle (another process, another daemon connection) reads
    // freely: the lock gates writers only, and staged work is invisible.
    let reader = Repository::open(&path).expect("open");
    assert!(observe_workspace(&reader) == pre_ws);
    assert!(observe_main(&reader) == pre_main);
    assert!(!reader.is_dirty().expect("dirty"));
    // A second writer is refused, not blocked.
    assert!(matches!(
        reader.begin_write().map(|_| ()),
        Err(GraphError::Locked { .. })
    ));

    // The save advances the workspace in one ref update: the next read sees it.
    tx.save().expect("save");
    let post_ws = observe_workspace(&reader);
    assert!(post_ws != pre_ws);
    assert!(post_ws.1.iter().any(|(k, _)| *k == node(500)));
    assert!(observe_main(&reader) == pre_main, "a save does not commit");
}

#[test]
fn reads_during_a_clean_merge_see_the_pre_or_post_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, repo) = init(dir.path());
    forked(&repo, false);
    let (pre_ws, pre_main) = (observe_workspace(&repo), observe_main(&repo));

    let readers = Readers::start(&path, 2);
    let outcome = repo.merge("other", "merge other").expect("merge");
    assert!(matches!(outcome, MergeOutcome::Merged(_)), "{outcome:?}");
    let (ws, main) = readers.finish();

    let (post_ws, post_main) = (observe_workspace(&repo), observe_main(&repo));
    assert!(post_ws == post_main, "a clean merge commits its workspace");
    assert_pre_or_post("workspace", &ws, &pre_ws, &post_ws);
    assert_pre_or_post("main", &main, &pre_main, &post_main);
}

#[test]
fn reads_during_a_conflicted_merge_and_its_abort_see_the_pre_or_post_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, repo) = init(dir.path());
    forked(&repo, true);
    let (clean_ws, main_tip) = (observe_workspace(&repo), observe_main(&repo));

    // The conflicted merge replaces the workspace with the partial merge;
    // the branch does not move.
    let readers = Readers::start(&path, 2);
    let outcome = repo.merge("other", "merge other").expect("merge");
    assert!(matches!(outcome, MergeOutcome::Conflicts(_)), "{outcome:?}");
    let (ws, main) = readers.finish();
    let merging_ws = observe_workspace(&repo);
    assert!(merging_ws != clean_ws);
    assert_pre_or_post("workspace", &ws, &clean_ws, &merging_ws);
    assert_pre_or_post("main", &main, &main_tip, &main_tip);

    // The abort puts the workspace back at the branch tip.
    let readers = Readers::start(&path, 2);
    repo.abort_merge().expect("abort");
    let (ws, main) = readers.finish();
    assert!(observe_workspace(&repo) == clean_ws);
    assert_pre_or_post("workspace", &ws, &merging_ws, &clean_ws);
    assert_pre_or_post("main", &main, &main_tip, &main_tip);
}

#[test]
fn reads_during_gc_succeed_and_see_unchanged_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, repo) = init(dir.path());
    forked(&repo, false);
    // A long-lived handle opened before any gc: its view of the packs goes
    // stale as each gc repacks and prunes, and it must still read.
    let long_lived = Repository::open(&path).expect("open");
    // Several rounds, each churning loose objects for gc to pack and prune
    // while readers race it: the prune of a loose object a reader is about
    // to open is the hazard (the store retries such a miss, acetone-d5is.2).
    for round in 0..4 {
        for v in 1..=10 {
            let mut tx = repo.begin_write().expect("begin");
            tx.put_node(&node(7), &record(round * 100 + v))
                .expect("put");
            tx.commit("churn", &[], None).expect("commit");
        }
        let (ws_state, main_state) = (observe_workspace(&repo), observe_main(&repo));

        let readers = Readers::start(&path, 3);
        repo.gc_with_hooks(
            // Reads proceed while gc holds the lock (the hook runs under it).
            || {
                assert!(observe_workspace(&long_lived) == ws_state);
                assert!(observe_main(&long_lived) == main_state);
            },
            || {},
        )
        .expect("gc");
        let (ws, main) = readers.finish();
        assert_pre_or_post("workspace", &ws, &ws_state, &ws_state);
        assert_pre_or_post("main", &main, &main_state, &main_state);
        // After the prune, the long-lived handle still reads everything.
        assert!(observe_workspace(&long_lived) == ws_state);
        assert!(observe_main(&long_lived) == main_state);
    }
}

/// Window 1, the conflicted merge: the workspace becomes the partial merge,
/// THEN `MERGE_HEAD` is written. Between the two a reader sees the partial
/// merge (with its conflicts map) and no `MERGE_HEAD`. The order is kept:
/// writing `MERGE_HEAD` first would let a crash leave it over a clean
/// workspace, turning the next ordinary commit into a spurious merge commit.
#[test]
fn window_conflicted_merge_workspace_before_merge_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, repo) = init(dir.path());
    let (_ours, theirs) = forked(&repo, true);
    assert!(matches!(
        repo.merge("other", "m").expect("merge"),
        MergeOutcome::Conflicts(_)
    ));
    let merging_ws = observe_workspace(&repo);
    // Reconstruct the in-between state: the merge's second step undone.
    repo.store()
        .delete_ref(MERGE_HEAD)
        .expect("drop merge head");

    let reader = Repository::open(&path).expect("open");
    // The graph read is the partial merge, consistent and complete.
    assert!(observe_workspace(&reader) == merging_ws);
    let snap = reader.workspace_snapshot().expect("snapshot");
    // The read path already treats it as a merge (the conflicts map is set),
    // so declared types are distrusted as they will be once MERGE_HEAD lands.
    assert!(snap.merge_in_progress());
    // The merge-head readers see none yet: status reports no merge block
    // and a dirty workspace; `conflicts` reports no merge in progress.
    assert_eq!(reader.merge_head().expect("merge head"), None);
    assert!(reader.is_dirty().expect("dirty"));
    assert!(matches!(reader.conflicts(), Err(GraphError::MergeState(_))));
    // Were the merge to stop here, commit refuses rather than commit the
    // partial merge as ordinary work …
    let tx = reader.begin_write().expect("begin");
    assert!(matches!(
        tx.commit("x", &[], None),
        Err(GraphError::MergeInProgress)
    ));

    // … and the merge's second step completes the state.
    repo.store()
        .write_ref(MERGE_HEAD, None, &theirs)
        .expect("write merge head");
    assert_eq!(reader.merge_head().expect("merge head"), Some(theirs));
    assert!(!reader.conflicts().expect("conflicts").is_empty());
}

/// Window 2, `abort_merge`: the workspace is reset to the branch tip, THEN
/// `MERGE_HEAD` is deleted. Between the two a reader sees the clean branch
/// tip with `MERGE_HEAD` still set. The order is kept: deleting `MERGE_HEAD`
/// first could leave the merged workspace (its conflicts map possibly
/// already empty) as ordinary uncommitted work that abort no longer accepts.
#[test]
fn window_abort_merge_workspace_before_merge_head_delete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, repo) = init(dir.path());
    let (_ours, theirs) = forked(&repo, true);
    let clean_ws = observe_workspace(&repo);
    assert!(matches!(
        repo.merge("other", "m").expect("merge"),
        MergeOutcome::Conflicts(_)
    ));
    // Reconstruct the in-between state: abort's first step alone (the
    // workspace reset, which leaves MERGE_HEAD untouched).
    repo.reset_workspace_to_head().expect("reset");

    let reader = Repository::open(&path).expect("open");
    // The graph read is already the post-abort state.
    assert!(observe_workspace(&reader) == clean_ws);
    assert!(!reader.is_dirty().expect("dirty"));
    // The merge state still reads as in progress, with nothing to resolve:
    // status reports a merge block with zero conflicts remaining, and the
    // read path distrusts declarations (a scan, never a different answer).
    assert_eq!(reader.merge_head().expect("merge head"), Some(theirs));
    assert!(
        reader
            .workspace_snapshot()
            .expect("snapshot")
            .merge_in_progress()
    );
    assert!(reader.conflicts().expect("conflicts").is_empty());

    // The abort's second step completes it (and re-running abort from this
    // state is the recovery should the delete fail).
    repo.abort_merge().expect("abort finishes");
    assert_eq!(reader.merge_head().expect("merge head"), None);
    assert!(
        !reader
            .workspace_snapshot()
            .expect("snapshot")
            .merge_in_progress()
    );
}

/// Window 3, the merge-completion commit: the branch advances to the merge
/// commit, THEN `MERGE_HEAD` is deleted. Between the two a reader sees the
/// completed merge with a stale `MERGE_HEAD` (already an ancestor of the
/// tip). The order is kept: deleting `MERGE_HEAD` first could let a crash
/// record the resolved merge as a single-parent commit, losing `theirs`.
#[test]
fn window_merge_completion_branch_before_merge_head_delete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, repo) = init(dir.path());
    let (ours, theirs) = forked(&repo, true);
    assert!(matches!(
        repo.merge("other", "m").expect("merge"),
        MergeOutcome::Conflicts(_)
    ));
    repo.resolve_all(ResolveSide::Ours).expect("resolve");
    let merge = repo
        .begin_write()
        .expect("begin")
        .commit("merge", &[], None)
        .expect("complete");
    let done_ws = observe_workspace(&repo);
    // Reconstruct the in-between state: the completion's last step undone.
    repo.store()
        .write_ref(MERGE_HEAD, None, &theirs)
        .expect("restore merge head");

    let reader = Repository::open(&path).expect("open");
    // The graph and history reads are the completed merge.
    assert_eq!(reader.head_commit().expect("head"), Some(merge));
    let parents = reader
        .store()
        .read_commit(&merge)
        .expect("read commit")
        .expect("merge commit")
        .parents;
    assert_eq!(parents, vec![ours, theirs]);
    assert!(observe_workspace(&reader) == done_ws);
    assert!(observe_main(&reader) == done_ws);
    assert!(!reader.is_dirty().expect("dirty"));
    // The stale MERGE_HEAD still reads as set: status reports a merge block
    // with zero conflicts (the stale guard derives none), and the read path
    // distrusts declarations meanwhile.
    assert_eq!(reader.merge_head().expect("merge head"), Some(theirs));
    assert!(reader.conflicts().expect("conflicts").is_empty());
    assert!(
        reader
            .workspace_snapshot()
            .expect("snapshot")
            .merge_in_progress()
    );

    // Should the delete have failed, the next commit clears the stale ref
    // and records an ordinary single-parent commit.
    let mut tx = reader.begin_write().expect("begin");
    tx.put_node(&node(900), &record(9)).expect("put");
    let next = tx.commit("next", &[], None).expect("commit");
    assert_eq!(reader.merge_head().expect("merge head"), None);
    let parents = reader
        .store()
        .read_commit(&next)
        .expect("read commit")
        .expect("commit")
        .parents;
    assert_eq!(parents, vec![merge]);
}
