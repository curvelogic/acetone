//! Property-based merge testing regime (acetone-14c.5). A generator produces
//! random *valid* base graphs (edges only between present nodes) and two
//! divergent edit scripts, then asserts the merge invariants across many
//! shrinkable cases:
//!
//! - **Determinism** (Invariant #4): merging the same inputs twice is
//!   byte-identical.
//! - **Symmetry**: a clean merge is direction-independent, and swapping the
//!   two sides never turns a clean merge into a conflicted one or vice versa.
//! - **No introduced dangling edges** (the acetone-14c.3 guarantee): when the
//!   inputs are referentially valid, a *clean* merge is too. This is checked
//!   by an independent scan of the merged graph — it does not trust the
//!   validator that produced the clean result.
//!
//! Cell-conflict side-swap symmetry is proven separately in `merge_prop.rs`;
//! this regime adds the edge-aware generator and the referential-integrity
//! property that `merge_prop.rs` (nodes only) cannot exercise. The generator
//! declares no schema, so the *constraint* dimension of merge validation
//! (existence, UNIQUE) is not exercised here — those branches are unit-tested
//! in `merge_validation.rs`.

use std::collections::{BTreeMap, BTreeSet};

use acetone_graph::merge::{ManifestMerge, merge_manifests};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::Manifest;
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_prolly::{get, scan};
use acetone_store::{GitStore, Hash};
use proptest::prelude::*;

/// A graph as generated: nodes (id -> value) and forward edges, with the
/// invariant that every edge endpoint is a present node.
type Graph = (BTreeMap<u8, i64>, BTreeSet<(u8, u8)>);

const IDS: u8 = 6;

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_string(), Value::Int(v))]))
}

fn edge((s, d): (u8, u8)) -> EdgeKey {
    EdgeKey::new(node(s), "R", node(d), Value::Null).expect("edge")
}

/// A referentially-valid graph: some nodes, and edges only between them.
fn valid_graph() -> impl Strategy<Value = Graph> {
    proptest::collection::btree_map(0u8..IDS, 0i64..3, 0..IDS as usize).prop_flat_map(|nodes| {
        let ids: Vec<u8> = nodes.keys().copied().collect();
        let pairs: Vec<(u8, u8)> = ids
            .iter()
            .flat_map(|&s| ids.iter().map(move |&d| (s, d)))
            .collect();
        let max = pairs.len().min(6);
        let edges = proptest::sample::subsequence(pairs, 0..=max)
            .prop_map(|v| v.into_iter().collect::<BTreeSet<(u8, u8)>>());
        (Just(nodes), edges)
    })
}

fn scenario() -> impl Strategy<Value = (Graph, Graph, Graph)> {
    (valid_graph(), valid_graph(), valid_graph())
}

fn init() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default()).expect("init");
    (dir, repo)
}

/// Reconcile the workspace from `prev` to `next` (delete what is gone, upsert
/// what remains or is new) and commit. Returns the commit and its manifest.
fn commit_delta(repo: &Repository, prev: &Graph, next: &Graph, message: &str) -> (Hash, Manifest) {
    let mut tx = repo.begin_write().expect("begin");
    for e in prev.1.difference(&next.1) {
        tx.delete_edge(&edge(*e)).expect("delete edge");
    }
    for id in prev.0.keys() {
        if !next.0.contains_key(id) {
            tx.delete_node(&node(*id)).expect("delete node");
        }
    }
    for (id, v) in &next.0 {
        tx.put_node(&node(*id), &record(*v)).expect("put node");
    }
    for e in &next.1 {
        tx.put_edge(&edge(*e), &EdgeRecord::default())
            .expect("put edge");
    }
    let commit = tx.commit(message, &[], None).expect("commit");
    let manifest = repo
        .snapshot(&commit.to_hex())
        .expect("snapshot")
        .manifest()
        .clone();
    (commit, manifest)
}

/// Independent referential-integrity check: does the graph in `manifest` have
/// a forward edge whose source or destination node is absent? Deliberately
/// re-implemented (not via the merge validator) so it cross-checks it.
fn has_dangling(store: &GitStore, manifest: &Manifest) -> bool {
    let params = manifest.chunk_params;
    let nodes = manifest.nodes.to_root(params).expect("nodes root");
    let edges = manifest.edges_fwd.to_root(params).expect("edges root");
    for item in scan(store, &edges, ..).expect("scan") {
        let (key, _) = item.expect("edge item");
        let e = EdgeKey::decode_fwd(&key).expect("decode edge");
        for endpoint in [e.src(), e.dst()] {
            let enc = endpoint.encode().expect("encode endpoint");
            if get(store, &nodes, &enc).expect("get node").is_none() {
                return true;
            }
        }
    }
    false
}

/// Whether two merge outcomes are byte-for-byte the same.
fn same_outcome(a: &ManifestMerge, b: &ManifestMerge) -> bool {
    match (a, b) {
        (ManifestMerge::Clean(x), ManifestMerge::Clean(y)) => x.encode() == y.encode(),
        (
            ManifestMerge::Conflicts { conflicts: x, .. },
            ManifestMerge::Conflicts { conflicts: y, .. },
        ) => x == y,
        _ => false,
    }
}

/// Build base / ours / theirs from three graphs and merge `ours`+`theirs`
/// over `base`. `ours` advances `main`; `theirs` is a branch forked at base.
fn build_and_merge(
    repo: &Repository,
    base: &Graph,
    ours: &Graph,
    theirs: &Graph,
) -> (Manifest, Manifest, Manifest) {
    let empty = (BTreeMap::new(), BTreeSet::new());
    let (base_commit, base_m) = commit_delta(repo, &empty, base, "base");
    let (_, ours_m) = commit_delta(repo, base, ours, "ours");
    repo.create_branch("other", Some(&base_commit.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let (_, theirs_m) = commit_delta(repo, base, theirs, "theirs");
    (base_m, ours_m, theirs_m)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn merge_regime((base, ours, theirs) in scenario()) {
        let (_dir, repo) = init();
        let (b, o, t) = build_and_merge(&repo, &base, &ours, &theirs);
        let store = repo.store();

        let forward = merge_manifests(store, &b, &o, &t).expect("merge");
        let again = merge_manifests(store, &b, &o, &t).expect("merge again");
        let reverse = merge_manifests(store, &b, &t, &o).expect("merge swapped");

        // Determinism: identical inputs, identical result.
        prop_assert!(same_outcome(&forward, &again), "merge is not deterministic");

        // Symmetry: swapping sides never changes clean-vs-conflicted, and a
        // clean merge is direction-independent.
        match (&forward, &reverse) {
            (ManifestMerge::Clean(f), ManifestMerge::Clean(r)) => {
                prop_assert_eq!(f.encode(), r.encode(), "clean merge must be direction-independent");
            }
            (ManifestMerge::Conflicts { .. }, ManifestMerge::Conflicts { .. }) => {}
            _ => prop_assert!(false, "swapping sides changed clean vs conflicted"),
        }

        // The acetone-14c.3 guarantee: inputs are referentially valid by
        // construction, so a clean merge must be too — verified independently.
        if let ManifestMerge::Clean(m) = &forward {
            prop_assert!(!has_dangling(store, m), "a clean merge introduced a dangling edge");
        }
    }
}

#[test]
fn has_dangling_detects_a_missing_endpoint() {
    // Positively self-test the independent oracle: property (3) only ever
    // calls `has_dangling` on clean merges, where it must return `false`, so
    // a silently-broken (always-false) detector would make the property pass
    // vacuously. Pin both answers here.
    let (_dir, repo) = init();
    let empty = (BTreeMap::new(), BTreeSet::new());
    let valid: Graph = (BTreeMap::from([(1, 0), (2, 0)]), BTreeSet::from([(1, 2)]));
    let (_c, valid_m) = commit_delta(&repo, &empty, &valid, "valid");
    assert!(
        !has_dangling(repo.store(), &valid_m),
        "a graph whose edge endpoints all exist is not dangling"
    );

    // Delete node 2 via plumbing while leaving edge 1 -> 2 in place: the
    // workspace manifest now holds an edge to an absent endpoint.
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(2)).expect("delete");
    let dangling_m = tx.save().expect("save");
    assert!(
        has_dangling(repo.store(), &dangling_m),
        "edge 1 -> 2 dangles once node 2 is deleted"
    );
}

#[test]
fn merging_a_branch_with_itself_is_a_noop() {
    // Identity law: merge(base, base, base) is clean and yields base exactly.
    let (_dir, repo) = init();
    let base: Graph = (BTreeMap::from([(1, 10), (2, 20)]), BTreeSet::from([(1, 2)]));
    let (b, o, t) = build_and_merge(&repo, &base, &base, &base);
    match merge_manifests(repo.store(), &b, &o, &t).expect("merge") {
        ManifestMerge::Clean(m) => {
            assert_eq!(
                m.encode(),
                b.encode(),
                "merging base with itself must reproduce base"
            );
        }
        other => panic!("expected a clean no-op merge, got {other:?}"),
    }
}
