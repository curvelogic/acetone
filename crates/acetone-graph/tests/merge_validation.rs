//! Post-merge graph validation (acetone-14c.3): a merge that is clean at the
//! map level can still break referential integrity or a schema constraint.
//! `merge_manifests` re-validates the merged graph over the changed key set
//! and surfaces any breach as a structured [`GraphViolation`] conflict — data,
//! not an error — leaving the repository untouched at the commit-graph level.

use acetone_graph::merge::{
    Endpoint, GraphViolation, ManifestMerge, MergeConflict, MergeOutcome, merge_manifests,
};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::Manifest;
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, SchemaEntry};
use std::collections::BTreeMap;
use std::path::Path;

fn init(dir: &Path) -> Repository {
    Repository::init(&dir.join("g.git"), InitOptions::default()).expect("init")
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn edge(s: u8, d: u8) -> EdgeKey {
    EdgeKey::new(node(s), "R", node(d), Value::Null).expect("edge")
}

fn record(props: &[(&str, Value)]) -> NodeRecord {
    NodeRecord::new(
        [],
        props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
    )
}

/// The manifest at `refspec` in `repo`.
fn manifest_at(repo: &Repository, refspec: &str) -> Manifest {
    repo.snapshot(refspec).expect("snapshot").manifest().clone()
}

/// Build base + two branches from edit closures, returning the three
/// manifests ready for `merge_manifests`. `ours` runs on `main`; `theirs`
/// runs on a fresh branch `other` forked at base.
fn diverge(
    repo: &Repository,
    base: impl FnOnce(&mut acetone_graph::repo::Transaction<'_>),
    ours: impl FnOnce(&mut acetone_graph::repo::Transaction<'_>),
    theirs: impl FnOnce(&mut acetone_graph::repo::Transaction<'_>),
) -> (Manifest, Manifest, Manifest) {
    let mut tx = repo.begin_write().expect("begin");
    base(&mut tx);
    let base_commit = tx.commit("base", &[], None).expect("commit");
    let base_m = manifest_at(repo, &base_commit.to_hex());

    let mut tx = repo.begin_write().expect("begin");
    ours(&mut tx);
    let ours_commit = tx.commit("ours", &[], None).expect("commit");
    let ours_m = manifest_at(repo, &ours_commit.to_hex());

    repo.create_branch("other", Some(&base_commit.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    theirs(&mut tx);
    let theirs_commit = tx.commit("theirs", &[], None).expect("commit");
    let theirs_m = manifest_at(repo, &theirs_commit.to_hex());

    (base_m, ours_m, theirs_m)
}

fn merge(repo: &Repository, b: &Manifest, o: &Manifest, t: &Manifest) -> ManifestMerge {
    merge_manifests(repo.store(), b, o, t).expect("merge")
}

fn violations(m: ManifestMerge) -> Vec<GraphViolation> {
    match m {
        ManifestMerge::Conflicts(conflicts) => conflicts
            .into_iter()
            .map(|c| match c {
                MergeConflict::Graph(v) => v,
                other => panic!("expected a graph violation, got {other:?}"),
            })
            .collect(),
        ManifestMerge::Clean(_) => panic!("expected conflicts, got a clean merge"),
    }
}

#[test]
fn ours_adds_edge_theirs_deletes_endpoint_is_a_dangling_conflict() {
    // The headline acceptance case: branch A deletes a node, branch B adds an
    // edge to it. Map-clean (different maps), but the merged graph dangles.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (b, o, t) = diverge(
        &repo,
        |tx| {
            for id in [1, 2] {
                tx.put_node(&node(id), &record(&[])).expect("put");
            }
        },
        // ours: add edge 1 -> 2.
        |tx| {
            tx.put_edge(&edge(1, 2), &EdgeRecord::default())
                .expect("edge");
        },
        // theirs: delete node 2 (the edge's destination).
        |tx| tx.delete_node(&node(2)).expect("delete"),
    );

    let vs = violations(merge(&repo, &b, &o, &t));
    assert_eq!(vs.len(), 1, "one dangling edge, got {vs:?}");
    match &vs[0] {
        GraphViolation::DanglingEdge {
            edge: e,
            endpoint,
            role,
        } => {
            assert_eq!(*role, Endpoint::Dst);
            assert_eq!(*e, edge(1, 2).encode_fwd().expect("enc"));
            assert_eq!(*endpoint, node(2).encode().expect("enc"));
        }
        other => panic!("expected DanglingEdge, got {other:?}"),
    }
}

#[test]
fn deleting_the_edge_source_also_dangles() {
    // Symmetric: the deleted node is the edge's *source*.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (b, o, t) = diverge(
        &repo,
        |tx| {
            for id in [1, 2] {
                tx.put_node(&node(id), &record(&[])).expect("put");
            }
        },
        |tx| {
            tx.put_edge(&edge(1, 2), &EdgeRecord::default())
                .expect("edge");
        },
        |tx| tx.delete_node(&node(1)).expect("delete"),
    );

    let vs = violations(merge(&repo, &b, &o, &t));
    assert_eq!(vs.len(), 1);
    assert!(matches!(
        &vs[0],
        GraphViolation::DanglingEdge {
            role: Endpoint::Src,
            ..
        }
    ));
}

#[test]
fn a_pre_existing_dangling_edge_is_not_attributed_to_the_merge() {
    // If base already contains a dangling edge (constructed via plumbing) and
    // neither side touches it, a disjoint clean merge must NOT report it.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (b, o, t) = diverge(
        &repo,
        // base: an edge 1 -> 2 but only node 1 exists (edge to a missing 2).
        |tx| {
            tx.put_node(&node(1), &record(&[])).expect("put");
            tx.put_edge(&edge(1, 2), &EdgeRecord::default())
                .expect("edge");
        },
        // ours: add an unrelated node 3.
        |tx| {
            tx.put_node(&node(3), &record(&[])).expect("put");
        },
        // theirs: add an unrelated node 4.
        |tx| {
            tx.put_node(&node(4), &record(&[])).expect("put");
        },
    );

    match merge(&repo, &b, &o, &t) {
        ManifestMerge::Clean(_) => {}
        ManifestMerge::Conflicts(c) => {
            panic!("pre-existing dangler must not be a merge conflict: {c:?}")
        }
    }
}

#[test]
fn cross_branch_unique_collision_is_a_conflict() {
    // Schema declares email UNIQUE on N; each side adds a different node with
    // the same email. Map-clean (distinct keys), but the merged graph breaks
    // the constraint.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let def = LabelDef::new(
        vec!["id".to_string()],
        BTreeMap::new(),
        [],
        ["email".to_string()],
    )
    .expect("label def");
    let (b, o, t) = diverge(
        &repo,
        |tx| {
            tx.put_schema(&SchemaEntry::Label {
                name: "N".to_string(),
                def: def.clone(),
            })
            .expect("schema");
        },
        |tx| {
            tx.put_node(&node(1), &record(&[("email", Value::String("a@x".into()))]))
                .expect("put");
        },
        |tx| {
            tx.put_node(&node(2), &record(&[("email", Value::String("a@x".into()))]))
                .expect("put");
        },
    );

    let vs = violations(merge(&repo, &b, &o, &t));
    assert_eq!(vs.len(), 1, "one unique collision, got {vs:?}");
    match &vs[0] {
        GraphViolation::UniqueViolation {
            label,
            property,
            nodes,
            ..
        } => {
            assert_eq!(label, "N");
            assert_eq!(property, "email");
            assert_eq!(
                nodes,
                &vec![
                    node(1).encode().expect("enc"),
                    node(2).encode().expect("enc")
                ]
            );
        }
        other => panic!("expected UniqueViolation, got {other:?}"),
    }
}

#[test]
fn distinct_unique_values_merge_cleanly() {
    // Same schema, but the two added nodes carry *different* emails — no
    // collision, so the merge is clean.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let def = LabelDef::new(
        vec!["id".to_string()],
        BTreeMap::new(),
        [],
        ["email".to_string()],
    )
    .expect("label def");
    let (b, o, t) = diverge(
        &repo,
        |tx| {
            tx.put_schema(&SchemaEntry::Label {
                name: "N".to_string(),
                def: def.clone(),
            })
            .expect("schema");
        },
        |tx| {
            tx.put_node(&node(1), &record(&[("email", Value::String("a@x".into()))]))
                .expect("put");
        },
        |tx| {
            tx.put_node(&node(2), &record(&[("email", Value::String("b@x".into()))]))
                .expect("put");
        },
    );

    assert!(matches!(merge(&repo, &b, &o, &t), ManifestMerge::Clean(_)));
}

#[test]
fn schema_tightening_flags_a_pre_existing_node_missing_the_new_required_property() {
    // ours adds a node without `email`; theirs tightens the schema to require
    // `email`. Merged: the schema requires it, the node lacks it -> breach.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let lax = LabelDef::new(vec!["id".to_string()], BTreeMap::new(), [], []).expect("lax");
    let strict = LabelDef::new(
        vec!["id".to_string()],
        BTreeMap::new(),
        ["email".to_string()],
        [],
    )
    .expect("strict");
    let (b, o, t) = diverge(
        &repo,
        |tx| {
            tx.put_schema(&SchemaEntry::Label {
                name: "N".to_string(),
                def: lax.clone(),
            })
            .expect("schema");
        },
        // ours: a node with no email (valid under the lax schema).
        |tx| {
            tx.put_node(&node(1), &record(&[])).expect("put");
        },
        // theirs: tighten the schema to require email.
        |tx| {
            tx.put_schema(&SchemaEntry::Label {
                name: "N".to_string(),
                def: strict.clone(),
            })
            .expect("schema");
        },
    );

    let vs = violations(merge(&repo, &b, &o, &t));
    assert_eq!(vs.len(), 1, "one missing-required, got {vs:?}");
    match &vs[0] {
        GraphViolation::MissingRequired { node: n, property } => {
            assert_eq!(*n, node(1).encode().expect("enc"));
            assert_eq!(property, "email");
        }
        other => panic!("expected MissingRequired, got {other:?}"),
    }
}

#[test]
fn validation_is_deterministic() {
    // Repeating the same merge yields byte-identical violations.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let (b, o, t) = diverge(
        &repo,
        |tx| {
            for id in [1, 2] {
                tx.put_node(&node(id), &record(&[])).expect("put");
            }
        },
        |tx| {
            tx.put_edge(&edge(1, 2), &EdgeRecord::default())
                .expect("edge");
        },
        |tx| tx.delete_node(&node(2)).expect("delete"),
    );
    let first = violations(merge(&repo, &b, &o, &t));
    let again = violations(merge(&repo, &b, &o, &t));
    assert_eq!(first, again);
}

#[test]
fn a_dangling_merge_writes_no_commit_at_the_repository_level() {
    // Through Repository::merge, a validation breach surfaces as
    // MergeOutcome::Conflicts and leaves the branch head untouched.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = init(dir.path());
    let mut tx = repo.begin_write().expect("begin");
    for id in [1, 2] {
        tx.put_node(&node(id), &record(&[])).expect("put");
    }
    let base = tx.commit("base", &[], None).expect("commit");

    repo.create_branch("other", Some(&base.to_hex()))
        .expect("branch");
    repo.checkout_branch("other").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.delete_node(&node(2)).expect("delete");
    tx.commit("theirs deletes 2", &[], None).expect("commit");

    repo.checkout_branch("main").expect("checkout");
    let mut tx = repo.begin_write().expect("begin");
    tx.put_edge(&edge(1, 2), &EdgeRecord::default())
        .expect("edge");
    let ours = tx.commit("ours adds 1->2", &[], None).expect("commit");

    match repo.merge("other", "merge other").expect("merge") {
        MergeOutcome::Conflicts(conflicts) => {
            assert_eq!(conflicts.len(), 1);
            assert!(matches!(
                &conflicts[0],
                MergeConflict::Graph(GraphViolation::DanglingEdge { .. })
            ));
        }
        other => panic!("expected Conflicts, got {other:?}"),
    }
    // The merge wrote nothing: main still points at `ours`.
    assert_eq!(repo.head_commit().expect("head"), Some(ours));
    assert!(!repo.is_dirty().expect("dirty"));
}
