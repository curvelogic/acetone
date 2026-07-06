//! Property test for Load-Bearing Invariant #4 (merge determinism): a
//! three-way merge is a pure function of `(base, ours, theirs)`, so it is
//! deterministic and — for a clean merge — symmetric: swapping `ours` and
//! `theirs` yields byte-identical merged roots (acetone-14c.2).

use acetone_graph::merge::{ManifestMerge, merge_manifests};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::manifest::Manifest;
use acetone_model::records::NodeRecord;
use proptest::prelude::*;
use std::collections::BTreeMap;

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_string(), Value::Int(v))]))
}

/// A base graph and two divergent edit scripts over a shared id space, so
/// overlapping edits produce genuine conflicts and disjoint edits merge
/// cleanly.
fn scenario() -> impl Strategy<Value = (BTreeMap<u8, i64>, Edits, Edits)> {
    let base = proptest::collection::btree_map(0u8..6, 0i64..3, 0..6);
    let edits = proptest::collection::btree_map(0u8..6, prop::option::of(0i64..3), 0..5);
    (base, edits.clone(), edits)
}

/// id -> Some(new value) to upsert, or None to delete.
type Edits = BTreeMap<u8, Option<i64>>;

/// Apply `edits` to the workspace, then commit; returns the new manifest.
fn apply_and_commit(repo: &Repository, edits: &Edits, message: &str) -> Manifest {
    let mut tx = repo.begin_write().expect("begin");
    for (id, edit) in edits {
        match edit {
            Some(v) => tx.put_node(&node(*id), &record(*v)).expect("put"),
            None => tx.delete_node(&node(*id)).expect("delete"),
        }
    }
    let commit = tx.commit(message, &[], None).expect("commit");
    repo.snapshot(&commit.to_hex())
        .expect("snapshot")
        .manifest()
        .clone()
}

/// Build base, then two branches editing it independently, and merge them.
/// The returned `ManifestMerge` owns its data (a `Manifest` is content
/// hashes, not a store borrow), so the temp repo is dropped on return.
fn three_way(base: &[(u8, i64)], ours: &Edits, theirs: &Edits) -> ManifestMerge {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default()).expect("init");
    let mut tx = repo.begin_write().expect("begin");
    for (id, v) in base {
        tx.put_node(&node(*id), &record(*v)).expect("put");
    }
    let base_commit = tx.commit("base", &[], None).expect("commit");
    let base_manifest = repo
        .snapshot(&base_commit.to_hex())
        .expect("s")
        .manifest()
        .clone();
    let ours_manifest = apply_and_commit(&repo, ours, "ours");
    let base_hex = base_commit.to_hex();
    repo.create_branch("theirs", Some(&base_hex))
        .expect("branch");
    repo.checkout_branch("theirs").expect("checkout");
    let theirs_manifest = apply_and_commit(&repo, theirs, "theirs");
    merge_manifests(
        repo.store(),
        &base_manifest,
        &ours_manifest,
        &theirs_manifest,
    )
    .expect("merge")
}

/// The manifest of a graph holding exactly `nodes`, built from scratch.
/// Identical content yields an identical manifest regardless of how it was
/// built (Load-Bearing Invariant #1), so this is a canonical oracle.
fn manifest_of(nodes: &[(u8, i64)]) -> Manifest {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default()).expect("init");
    let mut tx = repo.begin_write().expect("begin");
    for (id, v) in nodes {
        tx.put_node(&node(*id), &record(*v)).expect("put");
    }
    let commit = tx.commit("oracle", &[], None).expect("commit");
    repo.snapshot(&commit.to_hex())
        .expect("s")
        .manifest()
        .clone()
}

#[test]
fn disjoint_edits_merge_cleanly_to_the_union() {
    // base {1,2}; ours adds 3; theirs adds 4 -> clean union {1,2,3,4}.
    let merged = three_way(
        &[(1, 10), (2, 20)],
        &BTreeMap::from([(3, Some(30))]),
        &BTreeMap::from([(4, Some(40))]),
    );
    match merged {
        ManifestMerge::Clean(manifest) => {
            // History independence: the merged manifest equals the graph
            // built directly with all four nodes.
            let union = manifest_of(&[(1, 10), (2, 20), (3, 30), (4, 40)]);
            assert_eq!(manifest.encode(), union.encode());
        }
        ManifestMerge::Conflicts(c) => panic!("expected clean, got conflicts {c:?}"),
    }
}

#[test]
fn concurrent_edits_to_the_same_key_conflict() {
    // Both sides change node 1's value differently -> a conflict on its key.
    let merged = three_way(
        &[(1, 10)],
        &BTreeMap::from([(1, Some(11))]),
        &BTreeMap::from([(1, Some(12))]),
    );
    match merged {
        ManifestMerge::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].key, node(1).encode().expect("encode"));
        }
        ManifestMerge::Clean(_) => panic!("expected a conflict"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]
    #[test]
    fn merge_is_deterministic_and_symmetric((base, ours_edits, theirs_edits) in scenario()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default())
            .expect("init");

        // Base commit on main.
        let mut tx = repo.begin_write().expect("begin");
        for (id, v) in &base {
            tx.put_node(&node(*id), &record(*v)).expect("put");
        }
        let base_commit = tx.commit("base", &[], None).expect("commit base");
        let base_manifest = repo
            .snapshot(&base_commit.to_hex())
            .expect("snapshot")
            .manifest()
            .clone();

        // ours: edit on main from base.
        let ours = apply_and_commit(&repo, &ours_edits, "ours");

        // theirs: a fresh branch at base, edited independently.
        let base_hex = base_commit.to_hex();
        repo.create_branch("theirs", Some(&base_hex)).expect("branch");
        repo.checkout_branch("theirs").expect("checkout");
        let theirs = apply_and_commit(&repo, &theirs_edits, "theirs");

        let store = repo.store();
        let forward = merge_manifests(store, &base_manifest, &ours, &theirs).expect("merge");
        let reverse = merge_manifests(store, &base_manifest, &theirs, &ours).expect("merge swapped");
        // Determinism: the same inputs always give the same result.
        let again = merge_manifests(store, &base_manifest, &ours, &theirs).expect("merge again");

        match (forward, reverse, again) {
            (ManifestMerge::Clean(f), ManifestMerge::Clean(r), ManifestMerge::Clean(a)) => {
                let enc = |m: &Manifest| m.encode();
                // Symmetric: ours<->theirs swap yields identical roots.
                prop_assert_eq!(enc(&f), enc(&r), "clean merge must be direction-independent");
                // Deterministic: repeating the merge is byte-identical.
                prop_assert_eq!(enc(&f), enc(&a), "clean merge must be deterministic");
            }
            (ManifestMerge::Conflicts(f), ManifestMerge::Conflicts(r), ManifestMerge::Conflicts(a)) => {
                // The same keys conflict either way (base/ours/theirs values
                // swap sides, but the conflicted key set is symmetric).
                let keys = |c: &[acetone_graph::merge::MergeConflict]| {
                    c.iter().map(|x| (x.map, x.key.clone())).collect::<Vec<_>>()
                };
                prop_assert_eq!(keys(&f), keys(&r), "the conflicted key set is symmetric");
                prop_assert_eq!(keys(&f), keys(&a), "conflict detection is deterministic");
            }
            _ => prop_assert!(false, "the two merge directions disagreed on clean vs conflicted"),
        }
    }
}
