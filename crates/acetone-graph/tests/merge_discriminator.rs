//! The merge-time mirror of the declare-time discriminator stability check
//! (acetone-7qw.26, ADR-0082).
//!
//! `check_rel_disc_stability` refuses to change a relationship type's
//! discriminator while edges of that type exist — but only within one
//! workspace. A branch with no edges of the type may legally declare (or
//! rename, or drop) the discriminator while another branch writes edges under
//! the old declaration; the merge then pairs the two. Those edges must
//! surface as `GraphViolation::StaleDiscriminator` conflicts — data, not an
//! error — in both merge directions, deterministically, and block completion
//! until repaired.

use acetone_graph::GraphError;
use acetone_graph::merge::{
    GraphViolation, ManifestMerge, MergeConflict, MergeOutcome, merge_manifests,
};
use acetone_graph::repo::{InitOptions, Repository, Transaction};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::Manifest;
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, RelTypeDef, SchemaEntry};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("key")
}

fn links(disc: Option<&str>) -> SchemaEntry {
    SchemaEntry::RelType {
        name: "LINKS".into(),
        def: RelTypeDef::new(disc.map(str::to_owned), BTreeMap::new(), []).expect("rel type"),
    }
}

/// A `LINKS` edge from node 0 to `dst` with discriminator value `disc`.
fn link(dst: u8, disc: Value) -> EdgeKey {
    EdgeKey::new(node(0), "LINKS", node(dst), disc).expect("edge")
}

fn manifest_at(repo: &Repository, rev: &str) -> Manifest {
    repo.snapshot(rev).expect("snapshot").manifest().clone()
}

/// Which branch changes the discriminator; the other writes edges under the
/// base declaration.
#[derive(Debug, Clone, Copy)]
enum Declarer {
    Ours,
    Theirs,
}

/// Base: label `N`, nodes 0..=9, `LINKS` declared with `base_disc` and no
/// edges. The declaring branch redeclares `LINKS` with `new_disc` and then
/// adds `own` edges under it; the writing branch adds `stale` edges under
/// `base_disc`. Returns (base, ours, theirs) commit hexes, with `main` = ours
/// checked out and `other` = theirs.
fn diverge(
    repo: &Repository,
    declarer: Declarer,
    base_disc: Option<&str>,
    new_disc: Option<&str>,
    stale: &[EdgeKey],
    own: &[EdgeKey],
) -> (String, String, String) {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Label {
        name: "N".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("label");
    tx.put_schema(&links(base_disc)).expect("links");
    for id in 0..=9 {
        tx.put_node(&node(id), &NodeRecord::new([], BTreeMap::new()))
            .expect("node");
    }
    let base = tx.commit("base", &[], None).expect("base");

    let declare = |tx: &mut Transaction<'_>| {
        tx.put_schema(&links(new_disc)).expect("redeclare");
        for e in own {
            tx.put_edge(e, &EdgeRecord::default()).expect("own edge");
        }
    };
    let write = |tx: &mut Transaction<'_>| {
        for e in stale {
            tx.put_edge(e, &EdgeRecord::default()).expect("stale edge");
        }
    };

    let mut tx = repo.begin_write().expect("begin");
    match declarer {
        Declarer::Ours => declare(&mut tx),
        Declarer::Theirs => write(&mut tx),
    }
    let ours = tx.commit("ours", &[], None).expect("ours");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    match declarer {
        Declarer::Ours => write(&mut tx),
        Declarer::Theirs => declare(&mut tx),
    }
    let theirs = tx.commit("theirs", &[], None).expect("theirs");
    repo.checkout_branch("main").expect("checkout main");
    (base.to_hex(), ours.to_hex(), theirs.to_hex())
}

/// The pure merge of the three commits' manifests, as violations (empty for
/// a clean merge).
fn merge_violations(repo: &Repository, b: &str, o: &str, t: &str) -> Vec<GraphViolation> {
    let merged = merge_manifests(
        repo.store(),
        &manifest_at(repo, b),
        &manifest_at(repo, o),
        &manifest_at(repo, t),
    )
    .expect("merge");
    match merged {
        ManifestMerge::Clean(_) => Vec::new(),
        ManifestMerge::Conflicts { conflicts, .. } => conflicts
            .into_iter()
            .map(|c| match c {
                MergeConflict::Graph(v) => v,
                other => panic!("expected only graph violations, got {other:?}"),
            })
            .collect(),
    }
}

fn stale(edge: &EdgeKey, declared: Option<&str>, minted: Option<&str>) -> GraphViolation {
    GraphViolation::StaleDiscriminator {
        edge: edge.encode_fwd().expect("enc"),
        declared: declared.map(str::to_owned),
        minted: minted.map(str::to_owned),
    }
}

/// The bead's route, both directions: one branch declares a discriminator
/// while holding no `LINKS` edges (legal at declare time), the other writes
/// `Null`-keyed edges under the undiscriminated declaration. Each of those
/// edges is reported; the declaring side's own discriminated edges are not.
#[test]
fn declaring_a_discriminator_over_the_other_sides_edges_is_reported_both_ways() {
    for declarer in [Declarer::Ours, Declarer::Theirs] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init(dir.path());
        let stale_edges = [link(1, Value::Null), link(2, Value::Null)];
        let own_edges = [link(1, Value::Int(7)), link(3, Value::Int(8))];
        let (b, o, t) = diverge(&repo, declarer, None, Some("seq"), &stale_edges, &own_edges);
        let vs = merge_violations(&repo, &b, &o, &t);
        let expected: Vec<GraphViolation> = stale_edges
            .iter()
            .map(|e| stale(e, Some("seq"), None))
            .collect();
        assert_eq!(vs, expected, "{declarer:?} declares");
        // The one-line rendering names the edge and both declarations.
        let line = vs[0].to_string();
        assert!(
            line.contains("LINKS") && line.contains("no discriminator") && line.contains("seq"),
            "{line}"
        );
    }
}

/// A rename (`a` -> `b`) is invisible in the key shape — both declarations
/// mint non-null keys — so the check is by provenance, not shape.
#[test]
fn renaming_a_discriminator_over_the_other_sides_edges_is_reported() {
    for declarer in [Declarer::Ours, Declarer::Theirs] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init(dir.path());
        let stale_edges = [link(1, Value::Int(1))];
        let own_edges = [link(2, Value::Int(1))];
        let (b, o, t) = diverge(
            &repo,
            declarer,
            Some("a"),
            Some("b"),
            &stale_edges,
            &own_edges,
        );
        assert_eq!(
            merge_violations(&repo, &b, &o, &t),
            vec![stale(&stale_edges[0], Some("b"), Some("a"))],
            "{declarer:?} renames"
        );
    }
}

/// Dropping a discriminator (the wipe) while the other side writes
/// discriminated edges is the same hazard in reverse.
#[test]
fn dropping_a_discriminator_over_the_other_sides_edges_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let stale_edges = [link(1, Value::Int(1))];
    let (b, o, t) = diverge(&repo, Declarer::Theirs, Some("a"), None, &stale_edges, &[]);
    assert_eq!(
        merge_violations(&repo, &b, &o, &t),
        vec![stale(&stale_edges[0], None, Some("a"))]
    );
}

/// No discriminator change on either side: edges on both sides merge
/// cleanly, so the check adds no false positives (and no scan).
#[test]
fn an_unchanged_discriminator_merges_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (b, o, t) = diverge(
        &repo,
        Declarer::Ours,
        Some("seq"),
        Some("seq"),
        &[link(1, Value::Int(1))],
        &[link(2, Value::Int(2))],
    );
    assert!(merge_violations(&repo, &b, &o, &t).is_empty());
}

/// End to end through `Repository::merge`, both directions: the merge
/// enters merge-in-progress, `conflicts()` re-derives the violation live,
/// completion is refused naming it, and deleting the stale edge lets the
/// merge commit.
#[test]
fn a_stale_discriminator_blocks_completion_until_repaired_both_ways() {
    for declarer in [Declarer::Ours, Declarer::Theirs] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init(dir.path());
        let stale_edge = link(1, Value::Null);
        diverge(
            &repo,
            declarer,
            None,
            Some("seq"),
            std::slice::from_ref(&stale_edge),
            &[],
        );
        let expected = stale(&stale_edge, Some("seq"), None);
        match repo.merge("other", "merge other").expect("merge") {
            MergeOutcome::Conflicts(cs) => {
                assert_eq!(cs, vec![MergeConflict::Graph(expected.clone())]);
            }
            other => panic!("{declarer:?}: expected conflicts, got {other:?}"),
        }
        let live = repo.conflicts().expect("conflicts");
        assert_eq!(
            live,
            vec![acetone_graph::conflicts::WorkspaceConflict::Graph(
                expected.clone()
            )],
            "{declarer:?}"
        );
        let err = repo
            .begin_write()
            .expect("begin")
            .commit("finish", &[], None)
            .expect_err("completion must refuse");
        assert!(
            matches!(&err, GraphError::MergeViolations(vs) if vs == &vec![expected.clone()]),
            "{declarer:?}: {err:?}"
        );
        // Repair: delete the stale edge; the merge then completes.
        let mut tx = repo.begin_write().expect("begin");
        tx.delete_edge(&stale_edge).expect("delete");
        tx.commit("finish", &[], None)
            .expect("repaired merge completes");
        assert_eq!(repo.merge_head().expect("merge head"), None);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// Invariant #4 over the route: for any set of edges the writing side
    /// adds and any set the declaring side adds under the new
    /// discriminator, the merge reports exactly the writing side's edges —
    /// whichever side declares — and repeating the merge is byte-identical.
    #[test]
    fn exactly_the_other_sides_edges_are_reported_deterministically(
        // At least one: an empty branch has nothing to commit.
        stale_dsts in prop::collection::btree_set(1u8..=9, 1..5),
        own in prop::collection::btree_map(1u8..=9, 0i64..3, 0..5),
        declarer_is_ours in any::<bool>(),
    ) {
        let declarer = if declarer_is_ours { Declarer::Ours } else { Declarer::Theirs };
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init(dir.path());
        let stale_edges: Vec<EdgeKey> =
            stale_dsts.iter().map(|&d| link(d, Value::Null)).collect();
        let own_edges: Vec<EdgeKey> =
            own.iter().map(|(&d, &v)| link(d, Value::Int(v))).collect();
        let (b, o, t) = diverge(&repo, declarer, None, Some("seq"), &stale_edges, &own_edges);
        let first = merge_violations(&repo, &b, &o, &t);
        let again = merge_violations(&repo, &b, &o, &t);
        prop_assert_eq!(&first, &again);
        let reported: BTreeSet<Vec<u8>> = first
            .iter()
            .map(|v| match v {
                GraphViolation::StaleDiscriminator { edge, declared, minted } => {
                    assert_eq!(declared.as_deref(), Some("seq"));
                    assert_eq!(minted, &None);
                    edge.clone()
                }
                other => panic!("unexpected violation {other:?}"),
            })
            .collect();
        let expected: BTreeSet<Vec<u8>> =
            stale_edges.iter().map(|e| e.encode_fwd().expect("enc")).collect();
        prop_assert_eq!(reported, expected);
        // Edge-key order, each edge once.
        let order: Vec<Vec<u8>> = first
            .iter()
            .map(|v| match v {
                GraphViolation::StaleDiscriminator { edge, .. } => edge.clone(),
                _ => unreachable!(),
            })
            .collect();
        let mut sorted = order.clone();
        sorted.sort();
        sorted.dedup();
        prop_assert_eq!(order, sorted);
    }
}
