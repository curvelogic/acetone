//! History-rewrite engine tests (`acetone migrate`, acetone-hsg): the
//! generic `rewrite_history` engine exercised by the version-preserving
//! `Rechunk` transform. Verifies data preservation, faithful metadata,
//! ref/branch/tag rewriting, determinism/idempotence, and the guard rails.

use std::path::Path;
use std::process::Command;

use acetone_graph::merge::MergeOutcome;
use acetone_graph::repo::{InitOptions, Repository, WORKTREE_WORKSPACE_REF};
use acetone_graph::{MigrateJournal, MigrateReport, Rechunk, pending_migration, rewrite_history};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_prolly::{ChunkParams, scan};
use acetone_store::{ChunkStore, CommitStore, Hash, RefStore, RefSwing};

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

/// Run a git command in `repo`, panicking (with full output) on failure.
/// Tests MAY shell out to git to build and verify fixtures; library code
/// never does.
fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run a git command in `repo` with `stdin`, panicking on failure.
fn git_stdin(repo: &Path, args: &[&str], stdin: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn assert_fsck_clean(repo_git: &Path) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_git)
        .args(["fsck", "--strict"])
        .status()
        .expect("run git fsck");
    assert!(status.success(), "git fsck must be clean");
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
            tags_rewritten: 0,
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

#[test]
fn migrate_rewrites_annotated_tag_objects_preserving_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let repo_git = dir.path().join("graph.git");

    let c1 = put_range(&repo, 0, 100, "first");
    put_range(&repo, 100, 200, "second");
    // A genuine annotated tag, created by real git, at the first commit.
    git(
        &repo_git,
        &["tag", "-a", "ann", "-m", "release one", &c1.to_hex()],
    );

    let old_tag_id = target(&repo, "refs/tags/ann").expect("tag ref");
    assert_ne!(
        old_tag_id, c1,
        "the ref points at a tag object, not the commit"
    );
    let old_tag = repo
        .store()
        .read_tag(&old_tag_id)
        .expect("read_tag")
        .expect("a tag object");
    assert_eq!(old_tag.target, c1);
    assert!(old_tag.tagger.is_some(), "git records a tagger");

    let new_params = ChunkParams::new(512, 10, 8192).expect("params");
    let report = rewrite_history(&repo, &Rechunk::new(new_params)).expect("migrate");
    assert_eq!(report.tags_rewritten, 1);
    assert_eq!(report.refs_updated, 2, "main + the tag");

    // The ref points at a NEW tag object whose target is the rewritten first
    // commit (the parent of the rewritten head).
    let new_tag_id = target(&repo, "refs/tags/ann").expect("tag ref");
    assert_ne!(new_tag_id, old_tag_id, "the tag object was rewritten");
    let new_tag = repo
        .store()
        .read_tag(&new_tag_id)
        .expect("read_tag")
        .expect("still a tag object");
    let new_main = target(&repo, "refs/heads/main").expect("main");
    let new_c1 = repo
        .store()
        .read_commit(&new_main)
        .expect("read")
        .expect("present")
        .parents[0];
    assert_eq!(
        new_tag.target, new_c1,
        "tag repointed at the rewritten commit"
    );

    // Metadata preserved verbatim: name, message, tagger identity AND
    // timestamp.
    assert_eq!(new_tag.name, old_tag.name);
    assert_eq!(new_tag.message, old_tag.message);
    assert_eq!(new_tag.tagger, old_tag.tagger);
    assert!(!new_tag.signed);

    // git agrees: the ref still names an annotated tag and the repository is
    // fully connected.
    assert_eq!(
        git(&repo_git, &["cat-file", "-t", "refs/tags/ann"]).trim(),
        "tag"
    );
    assert_fsck_clean(&repo_git);

    // Idempotent: re-running leaves the rewritten tag where it is.
    rewrite_history(&repo, &Rechunk::new(new_params)).expect("migrate again");
    assert_eq!(target(&repo, "refs/tags/ann"), Some(new_tag_id));
}

#[test]
fn migrate_rewrites_nested_tag_chains() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let repo_git = dir.path().join("graph.git");

    let c1 = put_range(&repo, 0, 50, "first");
    // inner: tag -> commit; outer: tag -> tag -> commit (git permits tags of
    // tags: pointing `tag -a` at a tag ref tags the tag OBJECT, unpeeled).
    git(
        &repo_git,
        &["tag", "-a", "inner", "-m", "inner msg", &c1.to_hex()],
    );
    git(
        &repo_git,
        &["tag", "-a", "outer", "-m", "outer msg", "refs/tags/inner"],
    );

    let old_inner_id = target(&repo, "refs/tags/inner").expect("inner");
    let old_outer_id = target(&repo, "refs/tags/outer").expect("outer");
    let old_outer = repo
        .store()
        .read_tag(&old_outer_id)
        .expect("read")
        .expect("tag");
    assert_eq!(
        old_outer.target, old_inner_id,
        "outer wraps the inner tag object"
    );

    let new_params = ChunkParams::new(512, 10, 8192).expect("params");
    let report = rewrite_history(&repo, &Rechunk::new(new_params)).expect("migrate");
    // Two distinct tag objects; the inner one is shared between both refs'
    // chains and rewritten (and counted) once.
    assert_eq!(report.tags_rewritten, 2);

    let new_inner_id = target(&repo, "refs/tags/inner").expect("inner");
    let new_outer_id = target(&repo, "refs/tags/outer").expect("outer");
    assert_ne!(new_inner_id, old_inner_id);
    assert_ne!(new_outer_id, old_outer_id);

    // Chain shape preserved: outer -> (rewritten inner tag) -> rewritten
    // commit, and the inner ref points at that same rewritten inner object.
    let new_outer = repo
        .store()
        .read_tag(&new_outer_id)
        .expect("read")
        .expect("tag");
    assert_eq!(new_outer.target, new_inner_id);
    let new_inner = repo
        .store()
        .read_tag(&new_inner_id)
        .expect("read")
        .expect("tag");
    let new_main = target(&repo, "refs/heads/main").expect("main");
    assert_eq!(
        new_inner.target, new_main,
        "inner peels to the rewritten commit"
    );
    assert_eq!(new_inner.message, "inner msg\n");
    assert_eq!(new_outer.message, "outer msg\n");
    assert_fsck_clean(&repo_git);
}

#[test]
fn migrate_refuses_a_signed_tag_without_moving_anything() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    let repo_git = dir.path().join("graph.git");

    let c1 = put_range(&repo, 0, 20, "first");
    // A signed tag (hand-built: a real GPG setup is not needed for the
    // parser to see the signature block).
    let tag_content = format!(
        "object {}\ntype commit\ntag sealed\n\
         tagger T <t@example.invalid> 1700000000 +0000\n\nmsg\n\
         -----BEGIN PGP SIGNATURE-----\n\nAAAA\n-----END PGP SIGNATURE-----\n",
        c1.to_hex()
    );
    let tag_id = git_stdin(
        &repo_git,
        &["hash-object", "-t", "tag", "-w", "--stdin"],
        tag_content.as_bytes(),
    );
    let tag_id = tag_id.trim();
    git(&repo_git, &["update-ref", "refs/tags/sealed", tag_id]);

    let old_main = target(&repo, "refs/heads/main").expect("main");
    let params = ChunkParams::new(512, 10, 8192).expect("params");
    match rewrite_history(&repo, &Rechunk::new(params)) {
        Err(acetone_graph::GraphError::Migrate(msg)) => {
            assert!(msg.contains("refs/tags/sealed"), "names the ref: {msg}");
            assert!(msg.contains("signed"), "names the cause: {msg}");
        }
        other => panic!("expected a signed-tag refusal, got {other:?}"),
    }
    // Refused up front: nothing moved.
    assert_eq!(target(&repo, "refs/heads/main"), Some(old_main));
    assert_eq!(
        target(&repo, "refs/tags/sealed")
            .expect("still present")
            .to_hex(),
        tag_id
    );
    assert!(pending_migration(&repo).expect("pending").is_none());
}

#[test]
fn migrate_leaves_a_virtual_workspace_virtual() {
    // A worktree acetone has never written in has NO materialised workspace
    // ref (acetone-ayq, PR #168): the workspace is virtual, reading the
    // checked-out commit's manifest. Migrate must neither trip over the
    // absent ref nor needlessly materialise it — after the branch swing the
    // virtual workspace follows the rewritten head by construction.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(dir.path());
    put_range(&repo, 0, 200, "first");
    put_range(&repo, 200, 400, "second");
    let old_entries = nodes_entries(&repo);
    let old_main = target(&repo, "refs/heads/main").expect("main");

    // Reproduce the fresh-worktree state: no per-worktree workspace ref (a
    // fresh repo has no legacy ref either), workspace virtual and clean.
    repo.store()
        .delete_ref(WORKTREE_WORKSPACE_REF)
        .expect("drop the materialised workspace ref");
    assert!(target(&repo, WORKTREE_WORKSPACE_REF).is_none());
    assert!(!repo.is_dirty().expect("virtual workspace reads clean"));

    let new_params = ChunkParams::new(512, 10, 8192).expect("params");
    let report = rewrite_history(&repo, &Rechunk::new(new_params)).expect("migrate");
    assert_eq!(report.commits_rewritten, 2);

    // The branch swung; the workspace stayed virtual and reads the rewritten
    // head: new parameters, same data, still clean.
    assert_ne!(target(&repo, "refs/heads/main").expect("main"), old_main);
    assert!(
        target(&repo, WORKTREE_WORKSPACE_REF).is_none(),
        "migrate must not materialise a virtual workspace"
    );
    assert_eq!(
        repo.workspace_manifest().expect("manifest").chunk_params,
        new_params
    );
    assert_eq!(nodes_entries(&repo), old_entries, "data preserved");
    assert!(!repo.is_dirty().expect("is_dirty"));
    assert!(pending_migration(&repo).expect("pending").is_none());
    assert_fsck_clean(&dir.path().join("graph.git"));
}

/// The refs of a small multi-ref repository, for the crash-simulation tests.
struct RefState {
    main: Hash,
    feature: Hash,
    tag: Hash,
    workspace: Hash,
}

fn capture(repo: &Repository) -> RefState {
    RefState {
        main: target(repo, "refs/heads/main").expect("main"),
        feature: target(repo, "refs/heads/feature").expect("feature"),
        tag: target(repo, "refs/tags/v1").expect("v1"),
        workspace: target(repo, WORKTREE_WORKSPACE_REF).expect("workspace"),
    }
}

/// Build a branching repo, migrate it fully, then reconstruct the exact
/// on-disk state a crash mid-swing leaves behind (journal present, some refs
/// swung, some not), returning `(repo, old, new, params)`.
fn interrupted_migration(dir: &Path) -> (Repository, RefState, RefState, ChunkParams) {
    let repo = init_repo(dir);
    let c1 = put_range(&repo, 0, 200, "first");
    put_range(&repo, 200, 400, "second");
    repo.store()
        .write_ref("refs/tags/v1", None, &c1)
        .expect("tag");
    repo.create_branch("feature", None).expect("branch");
    repo.checkout_branch("feature").expect("checkout feature");
    put_range(&repo, 400, 460, "third");
    repo.checkout_branch("main").expect("checkout main");
    let old = capture(&repo);

    // A completed migration tells us what the journalled swing was.
    let new_params = ChunkParams::new(512, 10, 8192).expect("params");
    rewrite_history(&repo, &Rechunk::new(new_params)).expect("migrate");
    assert!(pending_migration(&repo).expect("pending").is_none());
    let new = capture(&repo);

    // Reconstruct the crash state: `feature` and `v1` were not yet swung
    // (put them back), `main` and the workspace were; the journal records
    // the full planned swing, exactly as `rewrite_history` writes it.
    let store = repo.store();
    store
        .write_ref("refs/heads/feature", Some(&new.feature), &old.feature)
        .expect("unswing feature");
    store
        .write_ref("refs/tags/v1", Some(&new.tag), &old.tag)
        .expect("unswing v1");
    let journal = MigrateJournal {
        swings: vec![
            RefSwing {
                name: "refs/heads/feature".into(),
                expected: Some(old.feature),
                new: new.feature,
            },
            RefSwing {
                name: "refs/heads/main".into(),
                expected: Some(old.main),
                new: new.main,
            },
            RefSwing {
                name: "refs/tags/v1".into(),
                expected: Some(old.tag),
                new: new.tag,
            },
            RefSwing {
                name: WORKTREE_WORKSPACE_REF.into(),
                expected: Some(old.workspace),
                new: new.workspace,
            },
        ],
    };
    let blob = store.put(&journal.encode()).expect("journal blob");
    store
        .write_ref(repo.namespace().migrate_journal_ref(), None, &blob)
        .expect("journal ref");
    (repo, old, new, new_params)
}

#[test]
fn interrupted_swing_is_detected_and_recovered_to_fully_new() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, old, new, params) = interrupted_migration(dir.path());

    // Detectably in-progress — never silently mixed.
    let pending = pending_migration(&repo)
        .expect("pending")
        .expect("a journal is present");
    assert_eq!(pending.swings.len(), 4);

    // Re-running migrate completes the journalled swing first, then the
    // migration itself is an idempotent no-op: the repo ends fully new.
    let report = rewrite_history(&repo, &Rechunk::new(params)).expect("recover + migrate");
    assert_eq!(report.commits_rewritten, 3);
    let after = capture(&repo);
    assert_eq!(after.main, new.main);
    assert_eq!(after.feature, new.feature);
    assert_eq!(after.tag, new.tag);
    assert_eq!(after.workspace, new.workspace);
    assert_ne!(after.feature, old.feature, "the unswung ref was completed");
    assert!(
        pending_migration(&repo).expect("pending").is_none(),
        "journal cleared"
    );
    assert!(!repo.is_dirty().expect("is_dirty"));
    assert_fsck_clean(&dir.path().join("graph.git"));
}

#[test]
fn recovery_refuses_a_ref_moved_while_the_migration_lay_interrupted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, old, new, params) = interrupted_migration(dir.path());

    // Someone moved `feature` to a third value (old main) while the
    // migration lay interrupted: completing the swing would discard that, so
    // recovery must refuse and keep the journal.
    repo.store()
        .write_ref("refs/heads/feature", Some(&old.feature), &old.main)
        .expect("move feature elsewhere");

    match rewrite_history(&repo, &Rechunk::new(params)) {
        Err(acetone_graph::GraphError::Migrate(msg)) => {
            assert!(msg.contains("interrupted"), "explains the state: {msg}");
            assert!(msg.contains("refs/heads/feature"), "names the ref: {msg}");
        }
        other => panic!("expected a recovery refusal, got {other:?}"),
    }
    // Nothing else moved; the journal is kept for the operator.
    assert_eq!(target(&repo, "refs/heads/main"), Some(new.main));
    assert_eq!(target(&repo, "refs/tags/v1"), Some(old.tag));
    assert!(pending_migration(&repo).expect("pending").is_some());
}
