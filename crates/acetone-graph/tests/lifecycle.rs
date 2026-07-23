//! Repository-lifecycle hardening (acetone-tqd, acetone-ayq, acetone-k78):
//! crash-recoverable checkout, read-only `open`, and the no-change commit
//! guard.

use acetone_graph::GraphError;
use acetone_graph::lock::WriteLock;
use acetone_graph::repo::{DEFAULT_BRANCH, InitOptions, Repository, WORKTREE_WORKSPACE_REF};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_store::RefStore;
use std::collections::BTreeMap;
use std::path::Path;

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

fn node(key: &str) -> NodeKey {
    NodeKey::new("Host", vec![Value::String(key.to_owned())]).expect("valid")
}

fn record(cores: i64) -> NodeRecord {
    NodeRecord::new(
        [],
        BTreeMap::from([("cores".to_owned(), Value::Int(cores))]),
    )
}

fn commit_node(repo: &Repository, key: &str, cores: i64, message: &str) -> acetone_store::Hash {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(key), &record(cores)).expect("put");
    tx.commit(message, &[], None).expect("commit")
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

// ---------------------------------------------------------------------------
// acetone-tqd: checkout's two-ref update is crash-recoverable
// ---------------------------------------------------------------------------

/// Build a repository wedged in checkout's failure window: the workspace ref
/// holds `feature`'s committed manifest while HEAD still names `main` — the
/// state a crash between checkout's workspace CAS and its `set_head` leaves.
/// Returns (repo, feature_commit).
fn wedged_checkout(dir: &Path) -> (Repository, acetone_store::Hash) {
    let repo = init_repo(dir);
    commit_node(&repo, "web1", 8, "base");
    repo.create_branch("feature", None).expect("branch");
    repo.checkout_branch("feature").expect("checkout feature");
    let feature = commit_node(&repo, "web2", 4, "feature work");
    // Simulate the interrupted second step by winding HEAD back to main:
    // the workspace ref keeps feature's manifest, HEAD names main again.
    repo.store()
        .set_head("HEAD", "refs/heads/main")
        .expect("wind HEAD back");
    (repo, feature)
}

#[test]
fn interrupted_checkout_recovers_by_rerunning_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, feature) = wedged_checkout(dir.path());

    // The wedged state reads dirty (workspace = feature content, HEAD = main).
    assert!(repo.is_dirty().expect("dirty"), "wedged state reads dirty");

    // Recovery: re-running the same checkout is idempotent — it detects the
    // workspace already holds feature's committed manifest and completes the
    // interrupted step (moving HEAD) instead of refusing with DirtyWorkspace.
    repo.checkout_branch("feature").expect("re-run recovers");
    assert_eq!(
        repo.current_branch().expect("branch"),
        Some("refs/heads/feature".to_owned())
    );
    assert_eq!(repo.head_commit().expect("head"), Some(feature));
    assert!(!repo.is_dirty().expect("dirty"), "converged state is clean");
}

#[test]
fn interrupted_checkout_still_guards_other_branches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = wedged_checkout(dir.path());

    // Checking out any branch whose manifest the workspace does NOT already
    // hold still refuses: the narrowed guard protects exactly the writes that
    // would change workspace content.
    match repo.checkout_branch(DEFAULT_BRANCH) {
        Err(GraphError::DirtyWorkspace) => {}
        other => panic!("expected DirtyWorkspace, got {other:?}"),
    }
}

#[test]
fn dirty_checkout_of_a_missing_branch_still_reports_dirty_workspace() {
    // Error precedence is unchanged: with uncommitted changes, checking out a
    // branch that does not exist reports DirtyWorkspace (the CLI's scripted
    // session relies on this on an unborn `main`).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node("web1"), &record(1)).expect("put");
    tx.save().expect("save");
    match repo.checkout_branch("no-such-branch") {
        Err(GraphError::DirtyWorkspace) => {}
        other => panic!("expected DirtyWorkspace, got {other:?}"),
    }
}

#[test]
fn checkout_between_branches_at_the_same_commit_stays_a_noop_for_the_workspace() {
    // Two branches at the same commit share a manifest, so checkout between
    // them is exactly the "workspace already at target" fast path.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let base = commit_node(&repo, "web1", 8, "base");
    repo.create_branch("twin", None).expect("branch");
    repo.checkout_branch("twin").expect("checkout twin");
    assert_eq!(repo.head_commit().expect("head"), Some(base));
    assert!(!repo.is_dirty().expect("dirty"));
    repo.checkout_branch(DEFAULT_BRANCH).expect("checkout back");
    assert_eq!(
        repo.current_branch().expect("branch"),
        Some("refs/heads/main".to_owned())
    );
}

// ---------------------------------------------------------------------------
// acetone-ayq: open() is read-only; a fresh worktree reads its checked-out
// commit and materialises its workspace ref on the first write
// ---------------------------------------------------------------------------

#[test]
fn open_of_a_fresh_worktree_writes_nothing_and_takes_no_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let git_path = dir.path().join("graph.git");
    let repo = init_repo(dir.path());
    commit_node(&repo, "web1", 8, "seed");

    let wt = dir.path().join("wt-fresh");
    if !git_ok(
        &git_path,
        &["worktree", "add", wt.to_str().unwrap(), DEFAULT_BRANCH],
    ) {
        eprintln!("SKIP open_of_a_fresh_worktree_writes_nothing: no `git worktree add`");
        return;
    }
    let wt_git_dir = std::path::PathBuf::from(git_out(&wt, &["rev-parse", "--absolute-git-dir"]));

    // Hold the worktree's single-writer lock across open: a read-only open
    // must not want it (previously the first open bootstrapped under the
    // lock and would fail Locked here).
    let held = WriteLock::acquire(&wt_git_dir).expect("hold writer lock");
    let wt_repo = Repository::open(&wt).expect("open is read-only, even while locked");
    drop(held);

    // No workspace ref was created: the worktree reads its checked-out
    // commit's committed state virtually.
    assert_eq!(
        wt_repo
            .store()
            .read_ref(WORKTREE_WORKSPACE_REF)
            .expect("read ref"),
        None,
        "open must not materialise the per-worktree workspace ref"
    );
    let snap = wt_repo.workspace_snapshot().expect("snapshot");
    assert!(
        snap.get_node(&node("web1")).expect("get").is_some(),
        "the virtual workspace reads the checked-out commit's data"
    );
    assert!(
        !wt_repo.is_dirty().expect("dirty"),
        "a fresh worktree reads clean"
    );

    // The first write materialises the ref (CAS-create under the writer lock).
    let mut tx = wt_repo.begin_write().expect("begin");
    tx.put_node(&node("web2"), &record(4)).expect("put");
    tx.save().expect("save");
    assert!(
        wt_repo
            .store()
            .read_ref(WORKTREE_WORKSPACE_REF)
            .expect("read ref")
            .is_some(),
        "the first write materialises the per-worktree workspace ref"
    );
    assert!(wt_repo.is_dirty().expect("dirty"), "the write is visible");
}

#[cfg(unix)]
#[test]
fn open_and_read_work_on_a_read_only_filesystem_copy() {
    use std::os::unix::fs::PermissionsExt;

    /// Recursively chmod `root`: directories 0o555/0o755, files 0o444/0o644.
    fn set_readonly(root: &Path, readonly: bool) {
        let meta = std::fs::symlink_metadata(root).expect("stat");
        if meta.file_type().is_symlink() {
            return;
        }
        if meta.is_dir() {
            // Recurse before locking the directory down; unlock it first on
            // the way back so entries are listable.
            if !readonly {
                std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod dir");
            }
            for entry in std::fs::read_dir(root).expect("read_dir") {
                set_readonly(&entry.expect("entry").path(), readonly);
            }
            let mode = if readonly { 0o555 } else { 0o755 };
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode))
                .expect("chmod dir");
        } else {
            let mode = if readonly { 0o444 } else { 0o644 };
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode))
                .expect("chmod file");
        }
    }

    /// Restores write permission on drop so the tempdir can be removed even
    /// when an assertion fails mid-test.
    struct RestorePerms(std::path::PathBuf);
    impl Drop for RestorePerms {
        fn drop(&mut self) {
            set_readonly(&self.0, false);
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let git_path = dir.path().join("graph.git");
    let repo = init_repo(dir.path());
    let seed = commit_node(&repo, "web1", 8, "seed");

    // A fresh linked worktree that acetone has never opened: the strongest
    // read-only case (no workspace ref exists to read).
    let wt = dir.path().join("wt-ro");
    let have_worktree = git_ok(
        &git_path,
        &["worktree", "add", wt.to_str().unwrap(), DEFAULT_BRANCH],
    );
    if !have_worktree {
        eprintln!("SKIP open_and_read_work_on_a_read_only_filesystem_copy: no `git worktree add`");
        return;
    }
    drop(repo);

    set_readonly(dir.path(), true);
    let _restore = RestorePerms(dir.path().to_path_buf());

    let wt_repo = Repository::open(&wt).expect("open a read-only repository");
    let snap = wt_repo.workspace_snapshot().expect("snapshot");
    assert!(snap.get_node(&node("web1")).expect("get").is_some());
    assert!(!wt_repo.is_dirty().expect("dirty"));
    assert_eq!(wt_repo.head_commit().expect("head"), Some(seed));
}

// ---------------------------------------------------------------------------
// acetone-k78: no-change commits are refused by default, opt-in via
// commit_allow_empty
// ---------------------------------------------------------------------------

#[test]
fn an_empty_root_commit_is_refused_by_default_but_allowed_by_opt_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    let txn = repo.begin_write().expect("begin");
    match txn.commit("empty root", &[], None) {
        Err(GraphError::NothingToCommit) => {}
        other => panic!("expected NothingToCommit, got {other:?}"),
    }

    let txn = repo.begin_write().expect("begin");
    let root = txn
        .commit_allow_empty("deliberate empty root", &[], None)
        .expect("opt-in allows the empty root commit");
    assert_eq!(repo.head_commit().expect("head"), Some(root));
    let entries = repo.log(None).expect("log");
    assert_eq!(entries.len(), 1);
    assert!(entries[0].parents.is_empty());
}

#[test]
fn a_no_change_commit_on_a_clean_workspace_is_refused_by_default() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let base = commit_node(&repo, "web1", 8, "base");

    let txn = repo.begin_write().expect("begin");
    match txn.commit("again", &[], None) {
        Err(GraphError::NothingToCommit) => {}
        other => panic!("expected NothingToCommit, got {other:?}"),
    }
    // The refusal changed nothing: same head, one commit, clean workspace.
    assert_eq!(repo.head_commit().expect("head"), Some(base));
    assert_eq!(repo.log(None).expect("log").len(), 1);
    assert!(!repo.is_dirty().expect("dirty"));

    // The opt-in mints the marker commit: same manifest, new commit, correct
    // parent.
    let txn = repo.begin_write().expect("begin");
    let marker = txn
        .commit_allow_empty("marker", &[], None)
        .expect("opt-in allows the empty commit");
    assert_ne!(marker, base);
    assert_eq!(repo.head_commit().expect("head"), Some(marker));
    let entries = repo.log(None).expect("log");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].parents, vec![base]);
    assert!(!repo.is_dirty().expect("dirty"), "an empty commit is clean");
}

#[test]
fn a_transaction_whose_ops_net_to_no_change_is_refused() {
    // The guard is manifest-level, not staged-op-level: a put immediately
    // undone by a delete in the same transaction leaves the manifest equal to
    // the parent's, so there is still nothing to commit.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    commit_node(&repo, "web1", 8, "base");

    let mut txn = repo.begin_write().expect("begin");
    txn.put_node(&node("ghost"), &record(1)).expect("put");
    txn.delete_node(&node("ghost")).expect("delete");
    match txn.commit("net nothing", &[], None) {
        Err(GraphError::NothingToCommit) => {}
        other => panic!("expected NothingToCommit, got {other:?}"),
    }
}

#[test]
fn nothing_to_commit_error_text_names_the_condition() {
    // The CLI surfaces this error verbatim; keep the recognisable prefix.
    let message = GraphError::NothingToCommit.to_string();
    assert!(
        message.starts_with("nothing to commit"),
        "unexpected message: {message}"
    );
}
