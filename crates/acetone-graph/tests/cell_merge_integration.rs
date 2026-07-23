//! End-to-end cell-wise (per-property) merge through `Repository::merge`
//! (ADR-0035, acetone-clm). The unit-level per-property/label three-way merge
//! is proven in `crate::cell_merge`; this file drives the whole pipeline —
//! prolly whole-record conflict → cell-wise refinement → partial merged record
//! written back → per-property conflicts persisted → per-property resolution
//! that preserves the auto-merged properties — and re-checks the load-bearing
//! invariants (merge determinism #4; derived index/edges_rev consistency #5).

use acetone_graph::fsck;
use acetone_graph::merge::{
    ConflictMap, ManifestMerge, MergeConflict, MergeOutcome, merge_manifests,
};
use acetone_graph::repo::{InitOptions, Repository, ResolveSide};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::Manifest;
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
use acetone_prolly::scan;
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn record(labels: &[&str], props: &[(&str, Value)]) -> NodeRecord {
    NodeRecord::new(
        labels.iter().map(|s| (*s).to_string()),
        props
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn s(text: &str) -> Value {
    Value::String(text.into())
}

/// Fork base → `other` (theirs) and `main` (ours), running each edit closure,
/// then merge `other` into `main`. Returns the [`MergeOutcome`].
fn diverge_and_merge(
    repo: &Repository,
    base: impl FnOnce(&mut acetone_graph::repo::Transaction<'_>),
    ours: impl FnOnce(&mut acetone_graph::repo::Transaction<'_>),
    theirs: impl FnOnce(&mut acetone_graph::repo::Transaction<'_>),
) -> MergeOutcome {
    let mut tx = repo.begin_write().expect("begin");
    base(&mut tx);
    let base_commit = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base_commit.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    theirs(&mut tx);
    tx.commit("theirs", &[], None).expect("commit");

    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    ours(&mut tx);
    tx.commit("ours", &[], None).expect("commit");

    repo.merge("other", "merge other").expect("merge")
}

/// The workspace node's property value, or `None` when the node or property is
/// absent.
fn prop(repo: &Repository, id: u8, name: &str) -> Option<Value> {
    let snap = repo.workspace_snapshot().expect("snapshot");
    snap.get_node(&node(id))
        .expect("get")
        .and_then(|r| r.properties().get(name).cloned())
}

#[test]
fn divergent_properties_on_one_node_auto_merge() {
    // The flagship case (exit criterion 4): import sets os_version on a node
    // while a human sets owner on the *same* node. Different properties, so the
    // merge is clean — no conflict — and both edits land.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());

    let outcome = diverge_and_merge(
        &repo,
        |tx| {
            tx.put_node(&node(1), &record(&[], &[("name", s("web"))]))
                .expect("put");
        },
        |tx| {
            tx.put_node(
                &node(1),
                &record(&[], &[("name", s("web")), ("owner", s("greg"))]),
            )
            .expect("put");
        },
        |tx| {
            tx.put_node(
                &node(1),
                &record(&[], &[("name", s("web")), ("os_version", s("12"))]),
            )
            .expect("put");
        },
    );

    assert!(
        matches!(
            outcome,
            MergeOutcome::Merged(_) | MergeOutcome::FastForward(_)
        ),
        "divergent-property edits must merge cleanly, got {outcome:?}"
    );
    assert_eq!(prop(&repo, 1, "owner"), Some(s("greg")));
    assert_eq!(prop(&repo, 1, "os_version"), Some(s("12")));
    assert_eq!(prop(&repo, 1, "name"), Some(s("web")));
    assert!(
        !fsck::check(&repo).expect("fsck").has_errors(),
        "auto-merged graph must be fsck-clean"
    );
}

#[test]
fn a_partial_conflict_keeps_auto_merged_properties_and_resolves_the_rest() {
    // One node, two properties edited: `owner` diverges (a genuine per-property
    // conflict), `os_version` is one-sided (auto-merged). Mid-merge the node is
    // present with the auto-merged `os_version`, the conflicted `owner` is
    // withheld, and resolving `--all-ours` restores it without disturbing the
    // auto-merged property.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());

    let outcome = diverge_and_merge(
        &repo,
        |tx| {
            tx.put_node(&node(1), &record(&[], &[("owner", s("base"))]))
                .expect("put");
        },
        |tx| {
            tx.put_node(
                &node(1),
                &record(&[], &[("owner", s("greg")), ("os_version", s("12"))]),
            )
            .expect("put");
        },
        |tx| {
            tx.put_node(&node(1), &record(&[], &[("owner", s("ci"))]))
                .expect("put");
        },
    );
    match outcome {
        MergeOutcome::Conflicts(c) => {
            assert_eq!(c.len(), 1, "exactly the owner property conflicts");
        }
        other => panic!("expected one property conflict, got {other:?}"),
    }

    // Mid-merge: the one-sided property is already merged in; the conflicted
    // one is withheld.
    assert_eq!(prop(&repo, 1, "os_version"), Some(s("12")));
    assert_eq!(prop(&repo, 1, "owner"), None);

    assert_eq!(repo.resolve_all(ResolveSide::Ours).expect("resolve"), 1);
    // Ours' owner is restored; the auto-merged os_version is untouched.
    assert_eq!(prop(&repo, 1, "owner"), Some(s("greg")));
    assert_eq!(prop(&repo, 1, "os_version"), Some(s("12")));

    let tx = repo.begin_write().expect("begin");
    tx.commit("complete", &[], None).expect("commit");
    assert!(repo.merge_head().expect("merge head").is_none());
    assert!(!fsck::check(&repo).expect("fsck").has_errors());
}

#[test]
fn resolve_theirs_picks_the_other_side_for_the_conflicted_property() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let outcome = diverge_and_merge(
        &repo,
        |tx| {
            tx.put_node(&node(1), &record(&[], &[("owner", s("base"))]))
                .expect("put");
        },
        |tx| {
            tx.put_node(
                &node(1),
                &record(&[], &[("owner", s("greg")), ("note", s("kept"))]),
            )
            .expect("put");
        },
        |tx| {
            tx.put_node(&node(1), &record(&[], &[("owner", s("ci"))]))
                .expect("put");
        },
    );
    assert!(matches!(outcome, MergeOutcome::Conflicts(_)));
    repo.resolve_all(ResolveSide::Theirs).expect("resolve");
    // Theirs deleted `note` relative to ours' add, but `note` is a one-sided
    // add (auto-merged), so it survives; only the conflicted `owner` follows
    // theirs.
    assert_eq!(prop(&repo, 1, "owner"), Some(s("ci")));
    assert_eq!(prop(&repo, 1, "note"), Some(s("kept")));
}

#[test]
fn a_property_deleted_on_one_side_and_changed_on_the_other_conflicts() {
    // Property-level delete-vs-modify: ours removes `owner`, theirs changes it.
    // The node's existence is not disputed, so the node survives; only `owner`
    // is a per-property conflict.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let outcome = diverge_and_merge(
        &repo,
        |tx| {
            tx.put_node(
                &node(1),
                &record(&[], &[("owner", s("base")), ("keep", s("k"))]),
            )
            .expect("put");
        },
        |tx| {
            // ours deletes `owner` (keeps only `keep`).
            tx.put_node(&node(1), &record(&[], &[("keep", s("k"))]))
                .expect("put");
        },
        |tx| {
            tx.put_node(
                &node(1),
                &record(&[], &[("owner", s("changed")), ("keep", s("k"))]),
            )
            .expect("put");
        },
    );
    match outcome {
        MergeOutcome::Conflicts(c) => assert_eq!(c.len(), 1),
        other => panic!("expected a property conflict, got {other:?}"),
    }
    // The node lives; `keep` is untouched, `owner` is withheld pending resolve.
    assert_eq!(prop(&repo, 1, "keep"), Some(s("k")));
    assert_eq!(prop(&repo, 1, "owner"), None);
}

#[test]
fn secondary_labels_merge_set_wise_through_a_repository_merge() {
    // ours adds label B, theirs adds label C, to the same node — set-wise
    // union, no conflict.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let outcome = diverge_and_merge(
        &repo,
        |tx| {
            tx.put_node(&node(1), &record(&["A"], &[("v", Value::Int(0))]))
                .expect("put");
        },
        |tx| {
            tx.put_node(&node(1), &record(&["A", "B"], &[("v", Value::Int(0))]))
                .expect("put");
        },
        |tx| {
            tx.put_node(&node(1), &record(&["A", "C"], &[("v", Value::Int(0))]))
                .expect("put");
        },
    );
    assert!(
        matches!(
            outcome,
            MergeOutcome::Merged(_) | MergeOutcome::FastForward(_)
        ),
        "set-wise label edits must merge cleanly, got {outcome:?}"
    );
    let snap = repo.workspace_snapshot().expect("snapshot");
    let labels = snap
        .get_node(&node(1))
        .expect("get")
        .expect("node present")
        .secondary_labels()
        .to_vec();
    assert_eq!(
        labels,
        vec!["A".to_string(), "B".to_string(), "C".to_string()]
    );
}

#[test]
fn edge_property_divergence_auto_merges() {
    // Edges carry the same cell-wise machinery: two branches set different
    // properties on the same edge → clean merge.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let e = EdgeKey::new(node(1), "R", node(2), Value::Null).expect("edge");
    let e2 = e.clone();
    let e3 = e.clone();

    let outcome = diverge_and_merge(
        &repo,
        |tx| {
            tx.put_node(&node(1), &record(&[], &[])).expect("put");
            tx.put_node(&node(2), &record(&[], &[])).expect("put");
            tx.put_edge(
                &e,
                &EdgeRecord::new(BTreeMap::from([("since".to_string(), Value::Int(2020))])),
            )
            .expect("edge");
        },
        |tx| {
            tx.put_edge(
                &e2,
                &EdgeRecord::new(BTreeMap::from([
                    ("since".to_string(), Value::Int(2020)),
                    ("weight".to_string(), Value::Int(5)),
                ])),
            )
            .expect("edge");
        },
        |tx| {
            tx.put_edge(
                &e3,
                &EdgeRecord::new(BTreeMap::from([
                    ("since".to_string(), Value::Int(2020)),
                    ("colour".to_string(), s("red")),
                ])),
            )
            .expect("edge");
        },
    );
    assert!(
        matches!(
            outcome,
            MergeOutcome::Merged(_) | MergeOutcome::FastForward(_)
        ),
        "divergent edge properties must merge cleanly, got {outcome:?}"
    );
    assert!(
        !fsck::check(&repo).expect("fsck").has_errors(),
        "merged edge graph (incl. edges_rev) must be fsck-clean"
    );
}

#[test]
fn a_cell_merged_indexed_property_stays_index_consistent() {
    // A one-sided change to an *indexed* property auto-merges (no conflict);
    // the derived index must be rebuilt to match the merged node exactly
    // (Invariant #5: merge == reindex). fsck's index-consistency check is the
    // independent oracle.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let def = LabelDef::new(vec!["id".to_string()], BTreeMap::new(), [], []).expect("label def");

    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Label {
        name: "N".into(),
        def,
    })
    .expect("schema");
    tx.put_schema(&SchemaEntry::Index {
        name: "by_region".into(),
        def: IndexDef::new("N", vec!["region".into()]).expect("index def"),
    })
    .expect("index");
    // Base node with region=base and owner=x, alongside the schema.
    tx.put_node(
        &node(1),
        &record(&[], &[("region", s("base")), ("owner", s("x"))]),
    )
    .expect("put");
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    // theirs changes owner (a *different*, non-indexed property).
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(
        &node(1),
        &record(&[], &[("region", s("base")), ("owner", s("g"))]),
    )
    .expect("put");
    tx.commit("theirs owner", &[], None).expect("commit");

    repo.checkout_branch("main").expect("checkout");
    // ours changes the indexed region.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(
        &node(1),
        &record(&[], &[("region", s("eu")), ("owner", s("x"))]),
    )
    .expect("put");
    tx.commit("ours region", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Merged(_) | MergeOutcome::FastForward(_) => {}
        other => panic!("a divergent-property merge must be clean, got {other:?}"),
    }
    // The merged node holds ours' indexed value and theirs' owner.
    assert_eq!(prop(&repo, 1, "region"), Some(s("eu")));
    assert_eq!(prop(&repo, 1, "owner"), Some(s("g")));

    // Index consistency: exactly one entry, region=eu → node 1, and fsck clean.
    assert!(
        !fsck::check(&repo).expect("fsck").has_errors(),
        "the cell-merged indexed property must leave the index consistent: {:?}",
        fsck::check(&repo).expect("fsck").findings
    );
    let manifest = repo.workspace_manifest().expect("manifest");
    let root = manifest
        .indexes
        .get("by_region")
        .expect("index present")
        .to_root(manifest.chunk_params)
        .expect("root");
    let count = scan(repo.store(), &root, ..).expect("scan").count();
    assert_eq!(count, 1, "one index entry for the single merged node");
}

// --- Determinism over the sorted property set (Invariant #4), exercising the
//     cell-wise auto-merge write-back path that single-property merge tests
//     (`merge_prop.rs`) cannot reach. ---

/// Per-node edit: for each of two properties `a`/`b`, `Some(Some(v))` sets it,
/// `Some(None)` deletes it, `None` leaves it untouched.
type PropEdit = (Option<Option<i64>>, Option<Option<i64>>);
type Edits = BTreeMap<u8, PropEdit>;

fn two_prop_record(a: Option<i64>, b: Option<i64>) -> NodeRecord {
    let mut props = BTreeMap::new();
    if let Some(a) = a {
        props.insert("a".to_string(), Value::Int(a));
    }
    if let Some(b) = b {
        props.insert("b".to_string(), Value::Int(b));
    }
    NodeRecord::new([], props)
}

/// Apply an edit script to the workspace and commit; return the manifest.
fn commit_edits(
    repo: &Repository,
    base: &BTreeMap<u8, (i64, i64)>,
    edits: &Edits,
    msg: &str,
) -> Manifest {
    let mut tx = repo.begin_write().expect("begin");
    for (id, (a, b)) in base {
        let (mut cur_a, mut cur_b) = (Some(*a), Some(*b));
        if let Some((ea, eb)) = edits.get(id) {
            if let Some(ea) = ea {
                cur_a = *ea;
            }
            if let Some(eb) = eb {
                cur_b = *eb;
            }
        }
        tx.put_node(&node(*id), &two_prop_record(cur_a, cur_b))
            .expect("put");
    }
    // An arbitrary edit script may be a no-op (base unchanged); the property
    // still needs the commit to exist, so opt in to the empty commit
    // (acetone-k78: `commit` refuses no-change commits by default).
    let commit = tx.commit_allow_empty(msg, &[], None).expect("commit");
    repo.snapshot(&commit.to_hex())
        .expect("snapshot")
        .manifest()
        .clone()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A three-way merge of two-property node records is a pure function of its
    /// inputs: repeating it is byte-identical (determinism), and swapping the
    /// two sides never changes whether it is clean nor — when clean — the merged
    /// roots (symmetry). Two-property records mean a node edited on different
    /// properties auto-merges, driving the write-back path.
    #[test]
    fn cell_wise_merge_is_deterministic_and_symmetric(
        base in proptest::collection::btree_map(0u8..4, (0i64..3, 0i64..3), 1..4),
        ours in proptest::collection::btree_map(
            0u8..4,
            (prop::option::of(prop::option::of(0i64..3)), prop::option::of(prop::option::of(0i64..3))),
            0..4,
        ),
        theirs in proptest::collection::btree_map(
            0u8..4,
            (prop::option::of(prop::option::of(0i64..3)), prop::option::of(prop::option::of(0i64..3))),
            0..4,
        ),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init(dir.path());
        let mut tx = repo.begin_write().expect("begin");
        for (id, (a, b)) in &base {
            tx.put_node(&node(*id), &two_prop_record(Some(*a), Some(*b))).expect("put");
        }
        let base_commit = tx.commit("base", &[], None).expect("commit");
        let base_m = repo.snapshot(&base_commit.to_hex()).expect("snap").manifest().clone();

        // Two independent branches off base.
        repo.create_branch("ours", Some(&base_commit.to_hex())).expect("branch");
        repo.checkout_branch("ours").expect("checkout");
        let ours_m = commit_edits(&repo, &base, &ours, "ours");

        repo.checkout_branch("main").expect("checkout");
        let theirs_m = commit_edits(&repo, &base, &theirs, "theirs");

        let store = repo.store();
        let fwd = merge_manifests(store, &base_m, &ours_m, &theirs_m).expect("merge");
        let again = merge_manifests(store, &base_m, &ours_m, &theirs_m).expect("merge");
        let swapped = merge_manifests(store, &base_m, &theirs_m, &ours_m).expect("merge");

        let classify = |m: &ManifestMerge| matches!(m, ManifestMerge::Clean(_));
        // Determinism: same inputs → identical classification and, for a clean
        // merge, byte-identical merged manifest.
        prop_assert_eq!(classify(&fwd), classify(&again));
        // Symmetry: swapping sides preserves clean-vs-conflict classification.
        prop_assert_eq!(classify(&fwd), classify(&swapped));

        // The partial merged manifest (which now carries the *auto-merged*
        // records written back on the conflict path) and the persisted conflict
        // *identities* — `(map, key, property)`, the only thing that reaches the
        // conflicts map, side-value-free — must be byte-identical when repeated
        // and swap-invariant. This is the one place the auto-merge write-back on
        // the conflict path is asserted (single-property `merge_prop.rs` never
        // reaches it).
        let partial = |m: &ManifestMerge| -> Option<Vec<u8>> {
            match m {
                ManifestMerge::Clean(x) => Some(x.encode()),
                ManifestMerge::Conflicts { merged, .. } => Some(merged.encode()),
            }
        };
        let ids = |m: &ManifestMerge| -> Vec<(ConflictMap, Vec<u8>, Option<String>)> {
            let cs = match m {
                ManifestMerge::Clean(_) => return Vec::new(),
                ManifestMerge::Conflicts { conflicts, .. } => conflicts,
            };
            let mut v: Vec<_> = cs
                .iter()
                .filter_map(|c| match c {
                    MergeConflict::Cell(cell) => {
                        Some((cell.map, cell.key.clone(), cell.property.clone()))
                    }
                    MergeConflict::Graph(_) => None,
                })
                .collect();
            v.sort();
            v
        };
        prop_assert_eq!(partial(&fwd), partial(&again), "merge is deterministic");
        prop_assert_eq!(
            partial(&fwd),
            partial(&swapped),
            "merged manifest is direction-independent"
        );
        prop_assert_eq!(ids(&fwd), ids(&again), "conflict identities are deterministic");
        prop_assert_eq!(
            ids(&fwd),
            ids(&swapped),
            "persisted conflict identities are direction-independent"
        );
    }
}
