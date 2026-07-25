//! Integration test for `gc` (spec §3.1, §7, ADR-0011): after churn, `gc`
//! consolidates the loose objects into a packfile (deltaing rewritten chunks
//! against their write-time predecessors) and reclaims space.

use std::path::Path;

use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{LabelDef, SchemaEntry};
use acetone_store::{CommitStore, RefStore};
use std::collections::BTreeMap;

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

/// Total size of every file under `dir`, recursively.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        let meta = std::fs::symlink_metadata(&path).expect("meta");
        if meta.is_dir() {
            total += dir_size(&path);
        } else {
            total += meta.len();
        }
    }
    total
}

#[test]
fn gc_reclaims_space_after_churn() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());

    // Declare a label and seed a body of nodes so the maps are multi-level.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        for i in 0..500 {
            let key = NodeKey::new("N", vec![Value::Int(i)]).expect("k");
            tx.put_node(
                &key,
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(0))])),
            )
            .expect("n");
        }
        tx.commit("seed", &[], None).expect("commit");
    }

    // Churn: many commits, each rewriting one node's value — every commit
    // rewrites the leaf (and interior) chunks on its root-to-leaf path,
    // producing loose objects that mostly differ by a little from predecessors.
    for round in 1..=80i64 {
        let mut tx = repo.begin_write().expect("begin");
        let key = NodeKey::new("N", vec![Value::Int(round % 500)]).expect("k");
        tx.put_node(
            &key,
            &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(round))])),
        )
        .expect("n");
        tx.commit(&format!("churn {round}"), &[], None)
            .expect("commit");
    }

    let git_dir = repo.store().git_dir().to_path_buf();
    let before = dir_size(&git_dir);

    let stats = repo.gc().expect("gc");
    assert!(stats.objects > 0, "gc packed nothing");
    // The write path recorded base hints, so churn-rewritten chunks delta
    // against their predecessors rather than being stored whole (ADR-0011).
    assert!(stats.deltas > 0, "no deltas — base hints were not recorded");

    let after = dir_size(&git_dir);
    assert!(
        after < before,
        "gc did not reclaim space: {before} → {after} bytes ({} objects, {} deltas)",
        stats.objects,
        stats.deltas
    );

    // The repository is still fully readable and intact after consolidation.
    let report = acetone_graph::fsck(&repo).expect("fsck");
    assert!(
        !report.has_errors(),
        "fsck errors after gc: {:?}",
        report.findings
    );
    let snapshot = repo.workspace_snapshot().expect("snap");
    assert_eq!(snapshot.nodes().expect("nodes").len(), 500);
}

#[test]
fn gc_preserves_uncommitted_workspace_state() {
    // The highest-stakes gc safety property: consolidation must treat the
    // workspace (uncommitted, staged-and-saved) state as reachable and never
    // prune the objects only it references.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        tx.commit("schema", &[], None).expect("commit");
    }
    // Stage a body of nodes and SAVE without committing — this lives only in
    // the workspace, not on any branch.
    {
        let mut tx = repo.begin_write().expect("begin");
        for i in 0..400i64 {
            let key = NodeKey::new("N", vec![Value::Int(i)]).expect("k");
            tx.put_node(
                &key,
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(i))])),
            )
            .expect("n");
        }
        tx.save().expect("save");
    }
    assert!(repo.is_dirty().expect("dirty"), "workspace should be dirty");

    // Consolidate + prune, then confirm the uncommitted nodes survive intact.
    repo.gc().expect("gc");
    let report = acetone_graph::fsck(&repo).expect("fsck");
    assert!(
        !report.has_errors(),
        "gc pruned live workspace objects: {:?}",
        report.findings
    );
    let snapshot = repo.workspace_snapshot().expect("snap");
    assert_eq!(
        snapshot.nodes().expect("nodes").len(),
        400,
        "uncommitted workspace nodes were lost by gc"
    );
}

#[test]
fn gc_with_linked_worktrees_packs_their_private_state() {
    // acetone-6g5.10: gc enumerates every linked worktree's private refs
    // as pack roots, so another worktree's uncommitted state survives
    // consolidation — the old blanket refusal is gone.
    let dir = tempfile::tempdir().expect("tmp");
    let main_git = dir.path().join("graph.git");
    let repo = Repository::init(&main_git, InitOptions::default()).expect("init");
    let base = {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        tx.commit("schema", &[], None).expect("commit")
    };

    let wt = dir.path().join("wt-live");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_git)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(base.to_hex())
        .status()
        .expect("run git worktree add");
    assert!(status.success(), "git worktree add failed");

    // Uncommitted state in the LINKED worktree.
    let wt_repo = Repository::open(&wt).expect("open worktree");
    {
        let mut tx = wt_repo.begin_write().expect("begin");
        for i in 0..100 {
            let key = NodeKey::new("N", vec![Value::Int(i)]).expect("k");
            tx.put_node(
                &key,
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(i))])),
            )
            .expect("n");
        }
        tx.save().expect("save");
    }
    assert!(wt_repo.is_dirty().expect("dirty"));

    // gc from the MAIN worktree succeeds and preserves the linked
    // worktree's saved-but-uncommitted work.
    repo.gc().expect("gc must succeed with linked worktrees");
    let snapshot = wt_repo.workspace_snapshot().expect("snap");
    assert_eq!(
        snapshot.nodes().expect("nodes").len(),
        100,
        "linked-worktree uncommitted nodes were lost by gc"
    );
    let report = acetone_graph::fsck(&repo).expect("fsck");
    assert!(
        !report.has_errors(),
        "gc broke the store: {:?}",
        report.findings
    );
}

#[test]
fn gc_packs_worktree_state_even_without_its_durability_anchor() {
    // The load-bearing proof that gc's linked-worktree ref enumeration
    // (acetone-6g5.10) protects private state on its own: delete the
    // worktree's ADR-0044 durability anchor — the belt — and gc must
    // still preserve the uncommitted workspace via the enumerated
    // `refs/worktree/*` roots — the braces.
    let dir = tempfile::tempdir().expect("tmp");
    let main_git = dir.path().join("graph.git");
    let repo = Repository::init(&main_git, InitOptions::default()).expect("init");
    let base = {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        tx.commit("schema", &[], None).expect("commit")
    };
    let wt = dir.path().join("wt-anchorless");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_git)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(base.to_hex())
        .status()
        .expect("run git worktree add");
    assert!(status.success(), "git worktree add failed");
    let wt_repo = Repository::open(&wt).expect("open worktree");
    {
        let mut tx = wt_repo.begin_write().expect("begin");
        for i in 0..60 {
            let key = NodeKey::new("N", vec![Value::Int(i)]).expect("k");
            tx.put_node(
                &key,
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(i))])),
            )
            .expect("n");
        }
        tx.save().expect("save");
    }
    // Remove the belt: every durability anchor this save mirrored.
    for (anchor, _) in repo
        .store()
        .list_refs("refs/acetone/worktree-anchors/")
        .expect("list anchors")
    {
        repo.store().delete_ref(&anchor).expect("delete anchor");
    }
    repo.gc().expect("gc succeeds");
    let snapshot = wt_repo.workspace_snapshot().expect("snap");
    assert_eq!(
        snapshot.nodes().expect("nodes").len(),
        60,
        "the enumerated private refs alone must keep the state alive"
    );
    // Survival alone would also hold for an object gc never saw (loose
    // copies of unpacked objects are not pruned); the enumeration is
    // only proven load-bearing if the workspace actually entered the
    // pack — i.e. its nodes-root chunk's loose copy was pruned.
    let nodes_root = wt_repo
        .workspace_manifest()
        .expect("manifest")
        .nodes
        .hash
        .to_hex();
    let loose = main_git
        .join("objects")
        .join(&nodes_root[..2])
        .join(&nodes_root[2..]);
    assert!(
        !loose.exists(),
        "workspace root chunk still loose — it was never packed, so the \
         enumeration did not protect it"
    );
}

#[test]
fn gc_never_deletes_the_last_copy_of_unenumerated_objects() {
    // The tripwire for gc's real safety floor (PR #212 review): pruning
    // removes only loose copies of objects in the freshly installed
    // pack, so a root the enumeration MISSES — here a linked worktree's
    // detached HEAD at a commit no ref references... is itself now
    // enumerated (worktree HEADs are roots), so go one step further and
    // fabricate an acetone commit referenced by NOTHING at all. Its
    // objects must still be present after gc. If this test ever fails,
    // someone added "prune unreachable loose objects" and every
    // un-enumerated per-worktree state class (refs/bisect/*, mid-gc
    // worktree adds) became silent data loss.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    let base = {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        tx.commit("seed", &[], None).expect("commit")
    };
    // An orphan commit: created directly in the store, referenced by no
    // ref, no worktree, no anchor.
    let manifest = repo.workspace_manifest().expect("manifest");
    let bytes = manifest.encode();
    let mut orphan = acetone_store::NewCommit::new(&bytes, "s", "orphan");
    let parents = vec![base];
    orphan.parents = &parents;
    let tip = repo.store().create_commit(&orphan).expect("create commit");

    repo.gc().expect("gc");
    assert!(
        matches!(repo.store().read_commit(&tip), Ok(Some(_))),
        "gc deleted the last copy of an object it never enumerated — \
         the prune-only-packed invariant is broken"
    );
}

#[test]
fn gc_keeps_a_worktree_that_appears_in_the_residual_window() {
    // acetone-dfh, the residual window: `git worktree add` respects no acetone
    // lock, so a worktree can still appear AFTER the under-lock re-check while
    // gc is sweeping. gc must be safe-by-default in that window — when in
    // doubt it keeps data: the late worktree's durability anchor (ADR-0044)
    // is not deleted (anchor deletion is gated on the worktree's directory
    // being absent at deletion time), and consolidation's pruning never
    // deletes the last copy of an object (loose copies are removed only once
    // the object is in the durably installed pack; prior packs only once
    // fully superseded). So the late worktree's saved-but-uncommitted work
    // must read back intact after gc completes.
    let dir = tempfile::tempdir().expect("tmp");
    let main_git = dir.path().join("graph.git");
    let repo = Repository::init(&main_git, InitOptions::default()).expect("init");
    let base = {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        for i in 0..50 {
            let key = NodeKey::new("N", vec![Value::Int(i)]).expect("k");
            tx.put_node(
                &key,
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(i))])),
            )
            .expect("n");
        }
        tx.commit("seed", &[], None).expect("commit")
    };

    let wt = dir.path().join("wt-late");
    let stats = repo
        .gc_with_hooks(
            || {},
            // The residual window: a real worktree add plus an acetone save
            // (which writes the worktree's durability anchor), after the
            // re-check passed, before the sweep.
            || {
                let status = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&main_git)
                    .args(["worktree", "add", "--detach"])
                    .arg(&wt)
                    .arg(base.to_hex())
                    .status()
                    .expect("run git worktree add");
                assert!(status.success(), "git worktree add failed");
                let wt_repo = Repository::open(&wt).expect("open late worktree");
                let mut tx = wt_repo.begin_write().expect("begin in worktree");
                for i in 100..200 {
                    let key = NodeKey::new("N", vec![Value::Int(i)]).expect("k");
                    tx.put_node(
                        &key,
                        &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(i))])),
                    )
                    .expect("n");
                }
                // Save, not commit: this state is reachable only through the
                // worktree's private refs and its common-dir anchor.
                tx.save().expect("save in late worktree");
            },
        )
        .expect("gc completes despite the late worktree");
    assert!(stats.objects > 0, "gc consolidated the main graph");

    // The late worktree's anchor survived the sweep: gc must not delete an
    // anchor whose worktree exists.
    let anchors_dir = main_git.join("refs/acetone/worktree-anchors");
    let anchors: Vec<_> = std::fs::read_dir(&anchors_dir)
        .expect("anchors dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(
        anchors.len(),
        1,
        "the late worktree's durability anchor must survive gc, got {anchors:?}"
    );

    // And its saved-but-uncommitted chunks are intact: reopen cold and read
    // the whole workspace back.
    let wt_repo = Repository::open(&wt).expect("reopen late worktree");
    let snapshot = wt_repo.workspace_snapshot().expect("workspace snapshot");
    assert_eq!(
        snapshot.nodes().expect("nodes").len(),
        150,
        "the late worktree's saved workspace must survive gc in full"
    );
    let report = acetone_graph::fsck(&wt_repo).expect("fsck");
    assert!(
        !report.has_errors(),
        "fsck errors in the late worktree after gc: {:?}",
        report.findings
    );
}

#[test]
fn gc_from_a_linked_worktree_packs_the_main_worktrees_state() {
    // PR #212 review Minor 3: the main worktree's private refs are
    // invisible from a linked worktree's ref store, so gc run FROM the
    // linked side must collect them explicitly (and does so through the
    // store's reduced-trust open, ADR-0034). Load-bearing assertion:
    // the main worktree's dirty workspace root chunk enters the pack.
    let dir = tempfile::tempdir().expect("tmp");
    let main_git = dir.path().join("graph.git");
    let repo = Repository::init(&main_git, InitOptions::default()).expect("init");
    let base = {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        tx.commit("seed", &[], None).expect("commit")
    };
    // Dirty workspace in the MAIN worktree.
    {
        let mut tx = repo.begin_write().expect("begin");
        for i in 0..40 {
            let key = NodeKey::new("N", vec![Value::Int(i)]).expect("k");
            tx.put_node(
                &key,
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(i))])),
            )
            .expect("n");
        }
        tx.save().expect("save");
    }
    let wt = dir.path().join("wt-runner");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&main_git)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(base.to_hex())
        .status()
        .expect("run git worktree add");
    assert!(status.success(), "git worktree add failed");

    let wt_repo = Repository::open(&wt).expect("open worktree");
    wt_repo.gc().expect("gc from the linked worktree");

    let snapshot = repo.workspace_snapshot().expect("snap");
    assert_eq!(
        snapshot.nodes().expect("nodes").len(),
        40,
        "main worktree state lost by gc run from a linked worktree"
    );
    let nodes_root = repo
        .workspace_manifest()
        .expect("manifest")
        .nodes
        .hash
        .to_hex();
    let loose = main_git
        .join("objects")
        .join(&nodes_root[..2])
        .join(&nodes_root[2..]);
    assert!(
        !loose.exists(),
        "main workspace root chunk still loose — never packed by the linked-worktree gc"
    );
}
