//! Property test for merges of **indexed** repositories (acetone-mqz,
//! Load-Bearing Invariant #5): the merged manifest's derived `idx/<name>`
//! maps must be exactly the maps a from-scratch reindex of the merged nodes
//! produces — root-for-root. `merge_prop.rs` / `merge_regime.rs` build no
//! indexed repositories, so this file adds declared indexes (one on an
//! ordinary property, one on a key property) to the generated graphs and
//! cross-checks the merge's index rebuild against an independent oracle: a
//! fresh repository loaded with the merged node set and reindexed.
//!
//! Both merge outcomes are covered — a clean merge, and the conflict path,
//! whose partially-merged manifest (conflicted keys absent) must still carry
//! indexes consistent with its partial node map.

use std::collections::BTreeMap;

use acetone_graph::merge::{ManifestMerge, merge_manifests};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::manifest::Manifest;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
use proptest::prelude::*;

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

/// A node's generated state: a value for `v`, and a `region` that may be
/// absent, null (exercising the index's null-blindness) or a small string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    Absent,
    Null,
    Named(u8),
}

fn record(v: i64, region: Region) -> NodeRecord {
    let mut props = BTreeMap::from([("v".to_string(), Value::Int(v))]);
    match region {
        Region::Absent => {}
        Region::Null => {
            props.insert("region".to_string(), Value::Null);
        }
        Region::Named(r) => {
            props.insert("region".to_string(), Value::String(format!("r{r}")));
        }
    }
    NodeRecord::new([], props)
}

fn region() -> impl Strategy<Value = Region> {
    prop_oneof![
        Just(Region::Absent),
        Just(Region::Null),
        (0u8..3).prop_map(Region::Named),
    ]
}

/// id -> Some((v, region)) to upsert, or None to delete.
type Edits = BTreeMap<u8, Option<(i64, Region)>>;

fn scenario() -> impl Strategy<Value = (BTreeMap<u8, (i64, Region)>, Edits, Edits)> {
    let base = proptest::collection::btree_map(0u8..6, (0i64..3, region()), 0..6);
    let edits =
        || proptest::collection::btree_map(0u8..6, prop::option::of((0i64..3, region())), 0..5);
    (base, edits(), edits())
}

/// The declared schema every generated repository carries: a keyed label,
/// an index on an ordinary property and an index on the key property (whose
/// value lives in the node key, not the record — the other sourcing path).
fn schema_entries() -> Vec<SchemaEntry> {
    vec![
        SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        },
        SchemaEntry::Index {
            name: "by_region".into(),
            def: IndexDef::new("N", vec!["region".into()]).expect("index"),
        },
        SchemaEntry::Index {
            name: "by_id".into(),
            def: IndexDef::new("N", vec!["id".into()]).expect("index"),
        },
    ]
}

fn init() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default()).expect("init");
    (dir, repo)
}

/// Apply `edits` to the workspace, then commit; returns the new manifest.
fn apply_and_commit(repo: &Repository, edits: &Edits, message: &str) -> Manifest {
    let mut tx = repo.begin_write().expect("begin");
    for (id, edit) in edits {
        match edit {
            Some((v, r)) => tx.put_node(&node(*id), &record(*v, *r)).expect("put"),
            None => tx.delete_node(&node(*id)).expect("delete"),
        }
    }
    // Arbitrary edit sets may be net-empty; opt in (acetone-k78).
    let commit = tx.commit_allow_empty(message, &[], None).expect("commit");
    repo.snapshot(&commit.to_hex())
        .expect("snapshot")
        .manifest()
        .clone()
}

/// The independent oracle: load `nodes` into a fresh repository declaring the
/// same schema, run a full `reindex`, and return the resulting index roots.
/// Identical map contents give identical roots regardless of how they were
/// built (Invariant #1), so this is exactly "what a from-scratch reindex of
/// the merged nodes produces".
fn reindexed_oracle(
    nodes: &[(NodeKey, NodeRecord)],
) -> BTreeMap<String, acetone_model::manifest::MapRoot> {
    let (_dir, repo) = init();
    let mut tx = repo.begin_write().expect("begin");
    for entry in schema_entries() {
        tx.put_schema(&entry).expect("schema");
    }
    for (key, rec) in nodes {
        tx.put_node(key, rec).expect("put");
    }
    tx.save().expect("save");
    repo.reindex().expect("reindex");
    repo.workspace_manifest().expect("manifest").indexes
}

/// Scan a manifest's node map back to decoded `(key, record)` pairs.
fn scan_nodes(repo: &Repository, manifest: &Manifest) -> Vec<(NodeKey, NodeRecord)> {
    let root = manifest
        .nodes
        .to_root(manifest.chunk_params)
        .expect("nodes root");
    let mut out = Vec::new();
    for item in acetone_prolly::scan(repo.store(), &root, ..).expect("scan") {
        let (key, value) = item.expect("item");
        out.push((
            NodeKey::decode(&key).expect("key"),
            NodeRecord::decode(&value).expect("record"),
        ));
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn merged_indexes_equal_a_from_scratch_reindex((base, ours_edits, theirs_edits) in scenario()) {
        let (_dir, repo) = init();

        // Base commit: the schema (label + two indexes) and the base nodes.
        let mut tx = repo.begin_write().expect("begin");
        for entry in schema_entries() {
            tx.put_schema(&entry).expect("schema");
        }
        for (id, (v, r)) in &base {
            tx.put_node(&node(*id), &record(*v, *r)).expect("put");
        }
        // Arbitrary bases may be empty; opt in (acetone-k78).
        let base_commit = tx.commit_allow_empty("base", &[], None).expect("commit");
        let base_manifest = repo
            .snapshot(&base_commit.to_hex())
            .expect("snapshot")
            .manifest()
            .clone();

        // ours on main; theirs on a branch forked at base.
        let ours = apply_and_commit(&repo, &ours_edits, "ours");
        repo.create_branch("theirs", Some(&base_commit.to_hex())).expect("branch");
        repo.checkout_branch("theirs").expect("checkout");
        let theirs = apply_and_commit(&repo, &theirs_edits, "theirs");

        let outcome = merge_manifests(repo.store(), &base_manifest, &ours, &theirs)
            .expect("merge");
        // Both outcomes carry a merged manifest whose indexes were rebuilt:
        // clean, or the partial merge (conflicted keys absent).
        let merged = match &outcome {
            ManifestMerge::Clean(m) => m,
            ManifestMerge::Conflicts { merged, .. } => merged,
        };

        // Invariant #5: the merge's index maps equal, root for root, what a
        // from-scratch reindex of exactly the merged nodes produces.
        let merged_nodes = scan_nodes(&repo, merged);
        let oracle = reindexed_oracle(&merged_nodes);
        prop_assert_eq!(
            &merged.indexes,
            &oracle,
            "merged index roots must equal an independent reindex of the merged nodes"
        );
        // Both declared indexes must be present (an empty result would make
        // the comparison vacuous).
        prop_assert!(merged.indexes.contains_key("by_region"));
        prop_assert!(merged.indexes.contains_key("by_id"));
    }
}

#[test]
fn wrapper_merge_of_an_indexed_repository_survives_reindex_unchanged() {
    // Deterministic end-to-end pin through the public wrapper: a clean merge
    // of an indexed repository leaves index roots that `reindex` reproduces
    // identically (Invariant #5) — and the index is genuinely non-empty, so
    // the equality is not vacuous.
    let (_dir, repo) = init();
    let mut tx = repo.begin_write().expect("begin");
    for entry in schema_entries() {
        tx.put_schema(&entry).expect("schema");
    }
    tx.put_node(&node(1), &record(10, Region::Named(0)))
        .expect("put");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(3), &record(30, Region::Named(2)))
        .expect("put");
    tx.commit("theirs", &[], None).expect("commit");
    repo.checkout_branch("main").expect("checkout main");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(&node(2), &record(20, Region::Named(1)))
        .expect("put");
    tx.commit("ours", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        acetone_graph::merge::MergeOutcome::Merged(_) => {}
        other => panic!("expected Merged, got {other:?}"),
    }

    let after_merge = repo.workspace_manifest().expect("manifest");
    // Non-vacuous: all three nodes carry a region, so `by_region` cannot be
    // the empty map.
    let empty = acetone_prolly::empty(repo.store(), after_merge.chunk_params).expect("empty");
    let empty_root = acetone_model::manifest::MapRoot::from_root(&empty);
    assert_ne!(
        after_merge.indexes.get("by_region"),
        Some(&empty_root),
        "the merged region index must not be empty"
    );

    repo.reindex().expect("reindex");
    let after_reindex = repo.workspace_manifest().expect("manifest");
    assert_eq!(
        after_merge.indexes, after_reindex.indexes,
        "reindex must reproduce the merge's index roots identically"
    );
}
