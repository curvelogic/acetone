//! Property test for Load-Bearing Invariant #4 (merge determinism): a
//! three-way merge is a pure function of `(base, ours, theirs)`, so it is
//! deterministic and — for a clean merge — symmetric: swapping `ours` and
//! `theirs` yields byte-identical merged roots (acetone-14c.2).

use acetone_graph::merge::{ManifestMerge, MergeConflict, merge_manifests};
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
        ManifestMerge::Conflicts { conflicts: c, .. } => {
            panic!("expected clean, got conflicts {c:?}")
        }
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
        ManifestMerge::Conflicts { conflicts, .. } => {
            assert_eq!(conflicts.len(), 1);
            let MergeConflict::Cell(cell) = &conflicts[0] else {
                panic!("expected a cell conflict, got {:?}", conflicts[0]);
            };
            assert_eq!(cell.key, node(1).encode().expect("encode"));
        }
        ManifestMerge::Clean(_) => panic!("expected a conflict"),
    }
}

#[test]
fn edge_merge_rebuilds_the_reverse_map_to_match_a_direct_build() {
    // Exercises the derived-map path (Invariant #5): merging divergent edge
    // additions must rebuild edges_rev to exactly what a direct build gives.
    use acetone_model::graph_keys::EdgeKey;
    use acetone_model::records::EdgeRecord;
    let edge = |s: u8, d: u8| EdgeKey::new(node(s), "R", node(d), Value::Null).expect("edge");

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default()).expect("init");
    // base: nodes 1..4 with edge 1->2.
    let mut tx = repo.begin_write().expect("begin");
    for id in [1, 2, 3, 4] {
        tx.put_node(&node(id), &record(0)).expect("put");
    }
    tx.put_edge(&edge(1, 2), &EdgeRecord::default())
        .expect("edge");
    let base_commit = tx.commit("base", &[], None).expect("commit");
    let base_m = repo
        .snapshot(&base_commit.to_hex())
        .expect("s")
        .manifest()
        .clone();

    // ours adds 1->3.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 3), &EdgeRecord::default())
        .expect("edge");
    let ours_commit = tx.commit("ours", &[], None).expect("commit");
    let ours_m = repo
        .snapshot(&ours_commit.to_hex())
        .expect("s")
        .manifest()
        .clone();

    // theirs (branch at base) adds 2->4.
    let base_hex = base_commit.to_hex();
    repo.create_branch("theirs", Some(&base_hex))
        .expect("branch");
    repo.checkout_branch("theirs").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(2, 4), &EdgeRecord::default())
        .expect("edge");
    let theirs_commit = tx.commit("theirs", &[], None).expect("commit");
    let theirs_m = repo
        .snapshot(&theirs_commit.to_hex())
        .expect("s")
        .manifest()
        .clone();

    let merged = merge_manifests(repo.store(), &base_m, &ours_m, &theirs_m).expect("merge");
    let ManifestMerge::Clean(manifest) = merged else {
        panic!("expected a clean merge");
    };

    // Oracle: a graph built directly with all three edges. The whole-manifest
    // encode covers edges_fwd AND the rebuilt edges_rev.
    let odir = tempfile::tempdir().expect("tempdir");
    let orepo = Repository::init(&odir.path().join("o.git"), InitOptions::default()).expect("init");
    let mut tx = orepo.begin_write().expect("begin");
    for id in [1, 2, 3, 4] {
        tx.put_node(&node(id), &record(0)).expect("put");
    }
    for (s, d) in [(1, 2), (1, 3), (2, 4)] {
        tx.put_edge(&edge(s, d), &EdgeRecord::default())
            .expect("edge");
    }
    let ocommit = tx.commit("oracle", &[], None).expect("commit");
    let oracle = orepo
        .snapshot(&ocommit.to_hex())
        .expect("s")
        .manifest()
        .clone();

    assert_eq!(
        manifest.encode(),
        oracle.encode(),
        "merged manifest (incl. rebuilt edges_rev) must equal a direct build"
    );
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
            (ManifestMerge::Conflicts { conflicts: f, .. }, ManifestMerge::Conflicts { conflicts: r, .. }, ManifestMerge::Conflicts { conflicts: a, .. }) => {
                use acetone_graph::merge::CellConflict;
                // Deterministic: repeating the merge gives identical conflicts.
                prop_assert_eq!(&f, &a, "conflict detection is deterministic");
                // This scenario only edits nodes, so every conflict is a cell
                // conflict. Symmetric with the side labels swapped: forward's
                // `ours` is reverse's `theirs` (14c.4's resolver relies on it).
                let forward_swapped: Vec<MergeConflict> = f
                    .iter()
                    .map(|c| match c {
                        MergeConflict::Cell(cell) => MergeConflict::Cell(CellConflict {
                            map: cell.map,
                            key: cell.key.clone(),
                            property: cell.property.clone(),
                            base: cell.base.clone(),
                            ours: cell.theirs.clone(),
                            theirs: cell.ours.clone(),
                        }),
                        other => other.clone(),
                    })
                    .collect();
                prop_assert_eq!(forward_swapped, r, "conflict side labelling swaps with direction");
            }
            _ => prop_assert!(false, "the two merge directions disagreed on clean vs conflicted"),
        }
    }
}
