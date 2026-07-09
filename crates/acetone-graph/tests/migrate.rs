//! History-rewrite engine tests (`acetone migrate`, acetone-hsg): the
//! generic `rewrite_history` engine exercised by the version-preserving
//! `Rechunk` transform. Verifies data preservation, faithful metadata,
//! ref/branch/tag rewriting, determinism/idempotence, and the guard rails.

use std::path::Path;

use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_graph::{MigrateReport, Rechunk, rewrite_history};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_prolly::{ChunkParams, scan};
use acetone_store::{ChunkStore, CommitStore, Hash, RefStore};

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

fn node(key: &str) -> NodeKey {
    NodeKey::new("Host", vec![Value::String(key.to_owned())]).expect("valid")
}

fn put_range(repo: &Repository, from: usize, to: usize, message: &str) -> Hash {
    let mut tx = repo.begin_write().expect("begin");
    for i in from..to {
        tx.put_node(
            &node(&format!("h{i:08}")),
            &NodeRecord::new([], Default::default()),
        )
        .expect("put node");
    }
    tx.commit(message, &[], None).expect("commit");
    repo.head_commit().expect("head").expect("committed")
}

/// The sorted `(key, value)` entries of the current workspace's `nodes` map.
fn nodes_entries(repo: &Repository) -> Vec<(Vec<u8>, Vec<u8>)> {
    let manifest = repo.workspace_manifest().expect("manifest");
    let root = manifest.nodes.to_root(manifest.chunk_params).expect("root");
    scan(repo.store(), &root, ..)
        .expect("scan")
        .map(|item| {
            let (k, v) = item.expect("entry");
            (k.to_vec(), v.to_vec())
        })
        .collect()
}

fn target(repo: &Repository, name: &str) -> Option<Hash> {
    repo.store().read_ref(name).expect("read_ref")
}

/// The number of commits reachable from `head` along first/other parents.
fn history_len(repo: &Repository, head: Hash) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![head];
    while let Some(h) = stack.pop() {
        if !seen.insert(h) {
            continue;
        }
        let c = repo
            .store()
            .read_commit(&h)
            .expect("read")
            .expect("present");
        stack.extend(c.parents);
    }
    seen.len()
}

#[test]
fn rechunk_migration_rewrites_history_preserving_data_and_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    // Two linear commits on main, a lightweight tag at the first, and a
    // feature branch with a third commit — a small branching history.
    let c1 = put_range(&repo, 0, 300, "first");
    let c2 = put_range(&repo, 300, 600, "second");
    repo.store()
        .write_ref("refs/tags/v1", None, &c1)
        .expect("tag");
    repo.create_branch("feature", None).expect("branch");
    repo.checkout_branch("feature").expect("checkout feature");
    let c3 = put_range(&repo, 600, 660, "third");
    repo.checkout_branch("main").expect("checkout main");

    let old_main = target(&repo, "refs/heads/main").expect("main");
    let old_feature = target(&repo, "refs/heads/feature").expect("feature");
    let old_tag = target(&repo, "refs/tags/v1").expect("tag");
    assert_eq!(old_main, c2);
    assert_eq!(old_feature, c3);
    assert_eq!(old_tag, c1);
    let old_entries = nodes_entries(&repo);
    let old_params = repo.workspace_manifest().expect("manifest").chunk_params;
    let old_author = repo
        .store()
        .read_commit(&c2)
        .expect("read")
        .expect("present")
        .author;

    // Re-chunk under different parameters: version-preserving, but every root
    // and commit hash changes.
    let new_params = ChunkParams::new(512, 10, 8192).expect("valid params");
    assert_ne!(old_params, new_params);
    let report = rewrite_history(&repo, &Rechunk::new(new_params)).expect("migrate");
    assert_eq!(
        report,
        MigrateReport {
            commits_rewritten: 3,
            refs_updated: 3,
        }
    );

    // Every ref moved to a new commit; the structure is preserved.
    let new_main = target(&repo, "refs/heads/main").expect("main");
    let new_feature = target(&repo, "refs/heads/feature").expect("feature");
    let new_tag = target(&repo, "refs/tags/v1").expect("tag");
    assert_ne!(new_main, old_main, "commit hashes must change");
    assert_ne!(new_feature, old_feature);
    assert_ne!(new_tag, old_tag);
    assert_eq!(history_len(&repo, new_main), 2, "main still has 2 commits");
    assert_eq!(history_len(&repo, new_feature), 3, "feature still has 3");

    // Chunk parameters changed; node data is byte-for-byte preserved.
    assert_eq!(
        repo.workspace_manifest().expect("manifest").chunk_params,
        new_params
    );
    assert_eq!(nodes_entries(&repo), old_entries, "node data preserved");

    // Metadata preserved: the new main commit keeps the old author identity
    // and timestamp, and its message.
    let new_head_commit = repo
        .store()
        .read_commit(&new_main)
        .expect("read")
        .expect("present");
    assert_eq!(new_head_commit.author, old_author);
    assert_eq!(new_head_commit.message.trim(), "second");

    // The rewritten repository is git-fsck clean.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path().join("graph.git"))
        .args(["fsck", "--strict"])
        .status()
        .expect("run git fsck");
    assert!(status.success(), "git fsck must be clean after migrate");

    // Idempotent: re-running the same migration is a no-op (deterministic
    // hashes; refs already point at the rewritten commits).
    let again = rewrite_history(&repo, &Rechunk::new(new_params)).expect("migrate again");
    assert_eq!(again.commits_rewritten, 3);
    assert_eq!(target(&repo, "refs/heads/main"), Some(new_main));
    assert_eq!(target(&repo, "refs/heads/feature"), Some(new_feature));
}

#[test]
fn migrate_rewrites_a_merge_commit_remapping_both_parents() {
    // The highest-risk history shape for a rewrite: a real two-parent merge
    // commit in a diamond. Both parents must be remapped to their rewrites and
    // the merged content preserved.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());

    let base = put_range(&repo, 0, 50, "base");
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout other");
    let theirs = put_range(&repo, 50, 100, "on other");
    repo.checkout_branch("main").expect("checkout main");
    let ours = put_range(&repo, 100, 150, "on main");
    let old_merge = match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Merged(h) => h,
        other => panic!("expected a merge commit, got {other:?}"),
    };
    // Sanity: the merge records both tips as parents.
    let old_parents = repo
        .store()
        .read_commit(&old_merge)
        .expect("read")
        .expect("present")
        .parents;
    assert_eq!(old_parents, vec![ours, theirs]);
    let old_entries = nodes_entries(&repo);
    let old_history = history_len(&repo, old_merge);

    let new_params = ChunkParams::new(512, 10, 8192).expect("params");
    rewrite_history(&repo, &Rechunk::new(new_params)).expect("migrate");

    let new_merge = target(&repo, "refs/heads/main").expect("main");
    assert_ne!(new_merge, old_merge, "the merge commit was rewritten");
    let new_parents = repo
        .store()
        .read_commit(&new_merge)
        .expect("read")
        .expect("present")
        .parents;
    assert_eq!(new_parents.len(), 2, "still a two-parent merge");
    assert!(
        new_parents.iter().all(|p| !old_parents.contains(p)),
        "both parents were remapped to their rewrites"
    );
    assert_eq!(
        history_len(&repo, new_merge),
        old_history,
        "commit-graph shape preserved"
    );
    assert_eq!(nodes_entries(&repo), old_entries, "merged data preserved");

    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path().join("graph.git"))
        .args(["fsck", "--strict"])
        .status()
        .expect("run git fsck");
    assert!(status.success(), "git fsck clean after merge migrate");
}

#[test]
fn migrate_refuses_a_dirty_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    put_range(&repo, 0, 10, "committed");
    // Stage an uncommitted change.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("uncommitted"),
            &NodeRecord::new([], Default::default()),
        )
        .expect("put");
        tx.save().expect("save without commit");
    }
    assert!(repo.is_dirty().expect("is_dirty"));

    let params = ChunkParams::new(512, 10, 8192).expect("params");
    assert!(matches!(
        rewrite_history(&repo, &Rechunk::new(params)),
        Err(acetone_graph::GraphError::DirtyWorkspace)
    ));
}

#[test]
fn migrate_reports_a_non_commit_ref_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    put_range(&repo, 0, 10, "committed");
    // A tag pointing at a blob (stand-in for an annotated-tag object): not a
    // commit, so migrate refuses rather than misreading it.
    let blob = repo.store().put(b"not a commit").expect("put blob");
    repo.store()
        .write_ref("refs/tags/bad", None, &blob)
        .expect("bad tag");

    let params = ChunkParams::new(512, 10, 8192).expect("params");
    match rewrite_history(&repo, &Rechunk::new(params)) {
        Err(acetone_graph::GraphError::NotACommit { name }) => {
            assert_eq!(name, "refs/tags/bad");
        }
        other => panic!("expected NotACommit, got {other:?}"),
    }
}
