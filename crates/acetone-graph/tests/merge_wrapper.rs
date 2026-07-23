//! Broader scenario coverage for the commit-graph merge wrapper
//! ([`Repository::merge`], acetone-5fh): the paths `merge_commit.rs` leaves
//! untested — merging from a bare commit hash, edge and schema cell
//! conflicts, unborn-branch adoption, criss-cross merge-base selection, and
//! merging into a checked-out branch other than `main`. Every scenario
//! asserts both the outcome shape and determinism (Invariant #4): a
//! conflicted merge is aborted and re-run to an identical conflict set; a
//! clean merge is rebuilt in a fresh repository to byte-identical manifest
//! content (the merge commit's own hash embeds a wall-clock timestamp, so
//! content — not commit identity — is what must reproduce).

use std::collections::BTreeMap;
use std::path::Path;

use acetone_graph::merge::{ConflictMap, MergeConflict, MergeOutcome};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_graph::{GraphError, fsck};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, SchemaEntry};
use acetone_store::CommitStore;

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_string(), Value::Int(v))]))
}

fn edge(s: u8, d: u8) -> EdgeKey {
    EdgeKey::new(node(s), "R", node(d), Value::Null).expect("edge")
}

fn edge_record(w: i64) -> EdgeRecord {
    EdgeRecord::new(BTreeMap::from([("w".to_string(), Value::Int(w))]))
}

/// Put/overwrite one node and commit; returns the new commit hash.
fn commit_node(repo: &Repository, id: u8, v: i64, message: &str) -> acetone_store::Hash {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(id), &record(v)).expect("put");
    tx.commit(message, &[], None).expect("commit")
}

/// The encoded manifest of the version at `refspec`.
fn manifest_bytes(repo: &Repository, refspec: &str) -> Vec<u8> {
    repo.snapshot(refspec)
        .expect("snapshot")
        .manifest()
        .encode()
}

// --- (1) merging from a bare commit hash -------------------------------------

#[test]
fn merging_a_bare_commit_hash_resolves_like_a_branch() {
    // `resolve_commit`'s hex branch: the merge target is given as a raw
    // commit address, not a ref name. Built twice in independent repositories
    // to pin content determinism.
    fn run() -> Vec<u8> {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init(dir.path());
        let base = commit_node(&repo, 1, 10, "base");
        repo.create_branch("other", Some(&base.to_hex()))
            .expect("branch");
        repo.checkout_branch("other").expect("checkout");
        let theirs = commit_node(&repo, 3, 30, "add 3 on other");
        repo.checkout_branch("main").expect("checkout main");
        commit_node(&repo, 2, 20, "add 2 on main");

        // Merge by bare hash — no branch name involved.
        let merged = match repo
            .merge(&theirs.to_hex(), "merge by hash")
            .expect("merge")
        {
            MergeOutcome::Merged(h) => h,
            other => panic!("expected Merged, got {other:?}"),
        };
        manifest_bytes(&repo, &merged.to_hex())
    }

    let first = run();
    // Shape: the merged content is the union, from a direct oracle build.
    let odir = tempfile::tempdir().expect("tempdir");
    let orepo = init(odir.path());
    let mut tx = orepo.begin_write().expect("begin");
    for (id, v) in [(1, 10), (2, 20), (3, 30)] {
        tx.put_node(&node(id), &record(v)).expect("put");
    }
    let oracle = tx.commit("oracle", &[], None).expect("commit");
    assert_eq!(
        first,
        manifest_bytes(&orepo, &oracle.to_hex()),
        "bare-hash merge content must equal a direct build of the union"
    );
    // Determinism: an independent rebuild merges to identical content.
    assert_eq!(
        first,
        run(),
        "bare-hash merge must be content-deterministic"
    );
}

#[test]
fn merging_an_unknown_hash_is_an_unresolved_refspec() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    commit_node(&repo, 1, 10, "base");
    // Well-formed hex that names no object: unresolvable, not a store error.
    let absent = "0".repeat(40);
    let err = repo.merge(&absent, "merge nothing").expect_err("must fail");
    assert!(
        matches!(err, GraphError::UnresolvedRefspec { ref refspec } if *refspec == absent),
        "expected UnresolvedRefspec, got {err:?}"
    );
}

// --- (2) edge cell conflicts -------------------------------------------------

/// Base: nodes 1, 2 and edge 1->2 with `w = 0`; both sides then rewrite the
/// edge via `mutate`.
fn edge_conflict_repo(
    dir: &Path,
    ours: Option<i64>,
    theirs: Option<i64>,
) -> (Repository, Vec<MergeConflict>) {
    let repo = init(dir);
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(1), &record(10)).expect("put");
    tx.put_node(&node(2), &record(20)).expect("put");
    tx.put_edge(&edge(1, 2), &edge_record(0)).expect("edge");
    let base = tx.commit("base", &[], None).expect("commit");

    let mutate = |value: Option<i64>, message: &str| {
        let mut tx = repo.begin_write().expect("begin");
        match value {
            Some(w) => tx.put_edge(&edge(1, 2), &edge_record(w)).expect("put"),
            None => tx.delete_edge(&edge(1, 2)).expect("delete"),
        }
        tx.commit(message, &[], None).expect("commit");
    };

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    mutate(theirs, "theirs");
    repo.checkout_branch("main").expect("checkout main");
    mutate(ours, "ours");

    let conflicts = match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(c) => c,
        other => panic!("expected Conflicts, got {other:?}"),
    };
    (repo, conflicts)
}

#[test]
fn concurrent_edge_property_edits_conflict_on_the_edges_map() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, conflicts) = edge_conflict_repo(dir.path(), Some(1), Some(2));

    // Shape: exactly one per-property cell conflict in the Edges map.
    assert_eq!(conflicts.len(), 1, "got {conflicts:?}");
    let MergeConflict::Cell(cell) = &conflicts[0] else {
        panic!("expected a cell conflict, got {:?}", conflicts[0]);
    };
    assert_eq!(cell.map, ConflictMap::Edges);
    assert_eq!(cell.key, edge(1, 2).encode_fwd().expect("encode"));
    assert_eq!(cell.property.as_deref(), Some("w"));
    assert!(cell.base.is_some() && cell.ours.is_some() && cell.theirs.is_some());
    assert_ne!(cell.ours, cell.theirs, "the two sides genuinely diverge");

    // The wrapper entered merge-in-progress, and the conflict persisted.
    assert!(repo.merge_head().expect("merge head").is_some());
    assert_eq!(repo.conflicts().expect("conflicts").len(), 1);

    // Determinism: abort, re-merge — the identical conflict set.
    repo.abort_merge().expect("abort");
    assert!(repo.merge_head().expect("merge head").is_none());
    match repo.merge("other", "merge again").expect("re-merge") {
        MergeOutcome::Conflicts(again) => {
            assert_eq!(conflicts, again, "edge conflicts must be deterministic");
        }
        other => panic!("expected Conflicts again, got {other:?}"),
    }
}

#[test]
fn edge_delete_versus_modify_is_a_whole_record_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, conflicts) = edge_conflict_repo(dir.path(), None, Some(2));

    assert_eq!(conflicts.len(), 1, "got {conflicts:?}");
    let MergeConflict::Cell(cell) = &conflicts[0] else {
        panic!("expected a cell conflict, got {:?}", conflicts[0]);
    };
    assert_eq!(cell.map, ConflictMap::Edges);
    assert_eq!(
        cell.property, None,
        "disputed existence stays a whole-record conflict"
    );
    assert!(cell.base.is_some(), "the edge existed in the base");
    assert!(cell.ours.is_none(), "ours deleted the edge");
    assert!(cell.theirs.is_some(), "theirs modified the edge");

    // Determinism via abort + re-merge.
    repo.abort_merge().expect("abort");
    match repo.merge("other", "merge again").expect("re-merge") {
        MergeOutcome::Conflicts(again) => assert_eq!(conflicts, again),
        other => panic!("expected Conflicts again, got {other:?}"),
    }
}

// --- (3) schema cell conflicts -----------------------------------------------

#[test]
fn divergent_schema_declarations_conflict_on_the_schema_map() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");

    let declare = |key: &str, message: &str| {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "X".into(),
            def: LabelDef::new(vec![key.into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        tx.commit(message, &[], None).expect("commit");
    };

    // Both sides declare the same new label with different key tuples: its
    // definition is disputed, an opaque whole-record schema conflict.
    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    declare("b", "theirs declares X(b)");
    repo.checkout_branch("main").expect("checkout main");
    declare("a", "ours declares X(a)");

    let conflicts = match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(c) => c,
        other => panic!("expected Conflicts, got {other:?}"),
    };
    assert_eq!(conflicts.len(), 1, "got {conflicts:?}");
    let MergeConflict::Cell(cell) = &conflicts[0] else {
        panic!("expected a cell conflict, got {:?}", conflicts[0]);
    };
    assert_eq!(cell.map, ConflictMap::Schema);
    assert_eq!(
        cell.property, None,
        "schema entries never refine to per-property conflicts"
    );
    assert!(
        cell.base.is_none(),
        "the label did not exist in the merge base"
    );
    assert!(cell.ours.is_some() && cell.theirs.is_some());
    assert_ne!(cell.ours, cell.theirs);

    // Merge-in-progress entered; determinism via abort + re-merge.
    assert!(repo.merge_head().expect("merge head").is_some());
    repo.abort_merge().expect("abort");
    match repo.merge("other", "merge again").expect("re-merge") {
        MergeOutcome::Conflicts(again) => assert_eq!(conflicts, again),
        other => panic!("expected Conflicts again, got {other:?}"),
    }
}

// --- (4) unborn-branch adoption ----------------------------------------------

#[test]
fn merging_into_an_unborn_branch_adopts_theirs_wholesale() {
    // The realistic shape: a fresh repository whose `main` has no commits yet
    // and a branch fetched from elsewhere (the stores are ordinary bare git
    // repositories, so `git fetch` is the transport). Merging that branch
    // fast-forwards by *creating* the branch ref (`expected = None`).
    let src_dir = tempfile::tempdir().expect("tempdir");
    let src = init(src_dir.path());
    let seed = commit_node(&src, 1, 10, "seed");

    let dst_dir = tempfile::tempdir().expect("tempdir");
    let dst = init(dst_dir.path());
    assert_eq!(
        dst.head_commit().expect("head"),
        None,
        "a fresh repository's default branch is unborn"
    );

    // Fetch the source's main into the fresh repository as `seed`.
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(dst_dir.path().join("g.git"))
        .arg("fetch")
        .arg(src_dir.path().join("g.git"))
        .arg("refs/heads/main:refs/heads/seed")
        .output()
        .expect("run git fetch");
    assert!(
        out.status.success(),
        "git fetch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    match dst.merge("seed", "adopt seed").expect("merge") {
        MergeOutcome::FastForward(h) => assert_eq!(h, seed, "adoption lands on theirs' tip"),
        other => panic!("expected FastForward, got {other:?}"),
    }
    // The unborn branch now exists, pointing at the adopted commit, with the
    // workspace matching and nothing dirty.
    assert_eq!(dst.head_commit().expect("head"), Some(seed));
    assert!(!dst.is_dirty().expect("dirty"));
    assert_eq!(
        dst.workspace_manifest().expect("ws").encode(),
        manifest_bytes(&dst, "seed")
    );
    let report = fsck(&dst).expect("fsck");
    assert!(
        !report.has_errors(),
        "fsck must be clean after adoption: {:?}",
        report.errors().collect::<Vec<_>>()
    );
    // Determinism/idempotence: re-merging the adopted branch is a no-op.
    match dst.merge("seed", "again").expect("merge again") {
        MergeOutcome::AlreadyUpToDate => {}
        other => panic!("expected AlreadyUpToDate, got {other:?}"),
    }
}

// --- (5) criss-cross histories -----------------------------------------------

#[test]
fn criss_cross_merge_base_is_the_min_hash_tie_break() {
    // Criss-cross: A and B each merge the other, then the two merge commits
    // merge. Their common-ancestor set has two maximal elements (A and B);
    // the documented tie-break picks the smaller hash, deterministically.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");
    let a = commit_node(&repo, 2, 20, "A on main");
    repo.create_branch("ba", Some(&a.to_hex())).expect("branch");
    repo.create_branch("bb", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("bb").expect("checkout");
    let b = commit_node(&repo, 3, 30, "B on bb");

    // First crossing: main merges B; bb merges A. Both are clean (disjoint
    // node additions), leaving two distinct two-parent merge commits with
    // identical content.
    repo.checkout_branch("main").expect("checkout main");
    let m1 = match repo.merge("bb", "M1 = A + B").expect("merge") {
        MergeOutcome::Merged(h) => h,
        other => panic!("expected Merged for M1, got {other:?}"),
    };
    repo.checkout_branch("bb").expect("checkout bb");
    let m2 = match repo.merge("ba", "M2 = B + A").expect("merge") {
        MergeOutcome::Merged(h) => h,
        other => panic!("expected Merged for M2, got {other:?}"),
    };
    assert_ne!(m1, m2, "the two crossings are distinct commits");

    // The merge base of the crossing tips: both A and B are maximal common
    // ancestors; the tie-break takes the smaller hash. Asked twice — the
    // choice is stable.
    let expected = a.min(b);
    let chosen = repo.merge_base(&m1, &m2).expect("merge base");
    assert_eq!(chosen, Some(expected), "min-hash tie-break");
    assert_eq!(
        repo.merge_base(&m1, &m2).expect("merge base again"),
        Some(expected),
        "the tie-break is deterministic"
    );

    // Second crossing: merging the two merge commits is a genuine three-way
    // over the tie-broken base and resolves cleanly to the union.
    repo.checkout_branch("main").expect("checkout main");
    let m3 = match repo.merge("bb", "M3").expect("merge") {
        MergeOutcome::Merged(h) => h,
        other => panic!("expected Merged for M3, got {other:?}"),
    };
    let commit = repo.store().read_commit(&m3).expect("read").unwrap();
    assert_eq!(commit.parents, vec![m1, m2]);

    let odir = tempfile::tempdir().expect("tempdir");
    let orepo = init(odir.path());
    let mut tx = orepo.begin_write().expect("begin");
    for (id, v) in [(1, 10), (2, 20), (3, 30)] {
        tx.put_node(&node(id), &record(v)).expect("put");
    }
    let oracle = tx.commit("oracle", &[], None).expect("commit");
    assert_eq!(
        manifest_bytes(&repo, &m3.to_hex()),
        manifest_bytes(&orepo, &oracle.to_hex()),
        "the criss-cross merge resolves to the union"
    );
}

// --- (6) merging into a branch other than main -------------------------------

#[test]
fn merging_into_a_non_main_branch_advances_only_that_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let base = commit_node(&repo, 1, 10, "base");
    repo.create_branch("dev", Some(&base.to_hex()))
        .expect("branch");
    repo.create_branch("feature", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("feature").expect("checkout");
    let feature_tip = commit_node(&repo, 3, 30, "feature work");
    repo.checkout_branch("dev").expect("checkout dev");
    let dev_tip = commit_node(&repo, 2, 20, "dev work");

    let merged = match repo
        .merge("feature", "merge feature into dev")
        .expect("merge")
    {
        MergeOutcome::Merged(h) => h,
        other => panic!("expected Merged, got {other:?}"),
    };

    // dev advanced to the merge commit, parents [dev, feature] in order.
    assert_eq!(repo.head_commit().expect("head"), Some(merged));
    let commit = repo.store().read_commit(&merged).expect("read").unwrap();
    assert_eq!(commit.parents, vec![dev_tip, feature_tip]);

    // The other branches did not move.
    assert_eq!(repo.resolve_commit("main").expect("main"), base);
    assert_eq!(
        repo.resolve_commit("feature").expect("feature"),
        feature_tip
    );
    assert!(!repo.is_dirty().expect("dirty"));

    // Content: the union of dev's and feature's edits over the base.
    let odir = tempfile::tempdir().expect("tempdir");
    let orepo = init(odir.path());
    let mut tx = orepo.begin_write().expect("begin");
    for (id, v) in [(1, 10), (2, 20), (3, 30)] {
        tx.put_node(&node(id), &record(v)).expect("put");
    }
    let oracle = tx.commit("oracle", &[], None).expect("commit");
    assert_eq!(
        manifest_bytes(&repo, &merged.to_hex()),
        manifest_bytes(&orepo, &oracle.to_hex())
    );
}
