//! Integration tests for declared property index maintenance (spec §3.3,
//! Invariant #5): transactional maintenance, `reindex` reproducing identical
//! roots, and the null/NaN-blind rule.

use std::collections::BTreeMap;
use std::path::Path;

use acetone_graph::fsck::{self, FindingKind};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::manifest::MapRoot;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
use acetone_store::{ChunkStore, RefStore};

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

fn node(label: &str, key: &str) -> NodeKey {
    NodeKey::new(label, vec![Value::String(key.to_owned())]).expect("valid")
}

fn record(pairs: &[(&str, Value)]) -> NodeRecord {
    NodeRecord::new(
        [],
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    )
}

fn declare_host_label(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(vec!["name".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("schema");
    tx.commit("declare Host", &[], None).expect("commit");
}

fn declare_region_index(repo: &Repository) {
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Index {
        name: "host_region".into(),
        def: IndexDef::new("Host", vec!["region".into()]).expect("index"),
    })
    .expect("schema");
    tx.commit("declare index", &[], None).expect("commit");
}

/// The (value, node-key-string) pairs currently in an index, for assertions.
fn index_contents(repo: &Repository, name: &str) -> Vec<(Value, String)> {
    let snap = repo.workspace_snapshot().expect("snap");
    snap.index_entries(name)
        .expect("entries")
        .into_iter()
        .map(|e| {
            let node = match &e.node().key()[0] {
                Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            };
            // These fixtures declare single-property indexes.
            (e.values()[0].clone(), node)
        })
        .collect()
}

#[test]
fn declaring_an_index_builds_it_from_existing_nodes() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);

    // Nodes exist BEFORE the index is declared.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "web1"),
            &record(&[("region", Value::String("eu".into()))]),
        )
        .expect("n");
        tx.put_node(
            &node("Host", "db1"),
            &record(&[("region", Value::String("us".into()))]),
        )
        .expect("n");
        tx.commit("nodes", &[], None).expect("commit");
    }

    declare_region_index(&repo);

    let mut got = index_contents(&repo, "host_region");
    got.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        got,
        vec![
            (Value::String("us".into()), "db1".into()),
            (Value::String("eu".into()), "web1".into()),
        ]
    );
}

#[test]
fn maintenance_tracks_inserts_updates_and_deletes() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    declare_region_index(&repo);

    // Insert.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "web1"),
            &record(&[("region", Value::String("eu".into()))]),
        )
        .expect("n");
        tx.commit("insert", &[], None).expect("commit");
    }
    assert_eq!(
        index_contents(&repo, "host_region"),
        vec![(Value::String("eu".into()), "web1".into())]
    );

    // Update the indexed property: old entry gone, new entry present.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "web1"),
            &record(&[("region", Value::String("ap".into()))]),
        )
        .expect("n");
        tx.commit("update", &[], None).expect("commit");
    }
    assert_eq!(
        index_contents(&repo, "host_region"),
        vec![(Value::String("ap".into()), "web1".into())]
    );

    // Delete the node: entry removed.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.delete_node(&node("Host", "web1")).expect("del");
        tx.commit("delete", &[], None).expect("commit");
    }
    assert!(index_contents(&repo, "host_region").is_empty());
}

#[test]
fn null_and_missing_values_are_not_indexed() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    declare_region_index(&repo);

    {
        let mut tx = repo.begin_write().expect("begin");
        // present value
        tx.put_node(
            &node("Host", "a"),
            &record(&[("region", Value::String("eu".into()))]),
        )
        .expect("n");
        // null value → null-blind
        tx.put_node(&node("Host", "b"), &record(&[("region", Value::Null)]))
            .expect("n");
        // property absent → no entry
        tx.put_node(&node("Host", "c"), &record(&[("other", Value::Int(1))]))
            .expect("n");
        tx.commit("mixed", &[], None).expect("commit");
    }
    assert_eq!(
        index_contents(&repo, "host_region"),
        vec![(Value::String("eu".into()), "a".into())]
    );
}

#[test]
fn nan_valued_property_is_not_indexed() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    // Index a float property.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "M".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("s");
        tx.put_schema(&SchemaEntry::Index {
            name: "m_score".into(),
            def: IndexDef::new("M", vec!["score".into()]).expect("idx"),
        })
        .expect("s");
        tx.commit("schema", &[], None).expect("commit");
    }
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(&node("M", "ok"), &record(&[("score", Value::Float(1.5))]))
            .expect("n");
        tx.put_node(
            &node("M", "nan"),
            &record(&[("score", Value::Float(f64::NAN))]),
        )
        .expect("n");
        tx.commit("scores", &[], None).expect("commit");
    }
    // Only the non-NaN score is indexed; the NaN node persisted fine.
    let contents = index_contents(&repo, "m_score");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].1, "ok");
    assert!(
        repo.workspace_snapshot()
            .expect("snap")
            .get_node(&node("M", "nan"))
            .expect("get")
            .is_some()
    );
}

#[test]
fn nan_nested_in_a_list_value_is_not_indexed_and_does_not_panic() {
    // A list-valued indexed property with a NaN element is unencodable anywhere
    // in the tuple; it must be skipped quietly (NaN-blind), not panic.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Index {
            name: "host_scores".into(),
            def: IndexDef::new("Host", vec!["scores".into()]).expect("idx"),
        })
        .expect("s");
        tx.commit("index", &[], None).expect("commit");
    }
    {
        let mut tx = repo.begin_write().expect("begin");
        // A clean list is indexed; a list with a nested NaN is skipped.
        tx.put_node(
            &node("Host", "ok"),
            &record(&[(
                "scores",
                Value::List(vec![Value::Float(1.0), Value::Float(2.0)]),
            )]),
        )
        .expect("n");
        tx.put_node(
            &node("Host", "nan"),
            &record(&[(
                "scores",
                Value::List(vec![Value::Float(1.0), Value::Float(f64::NAN)]),
            )]),
        )
        .expect("n");
        tx.commit("scores", &[], None).expect("commit");
    }
    // Only the clean-list node is indexed; both nodes persisted.
    let contents = index_contents(&repo, "host_scores");
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].1, "ok");
    // reindex agrees (Invariant #5) and also does not panic.
    repo.reindex().expect("reindex");
    assert_eq!(index_contents(&repo, "host_scores").len(), 1);
}

#[test]
fn reindex_reproduces_identical_roots() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    declare_region_index(&repo);

    // A sequence of mutations maintained incrementally.
    for (name, region) in [("web1", "eu"), ("db1", "us"), ("cache1", "eu")] {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", name),
            &record(&[("region", Value::String(region.into()))]),
        )
        .expect("n");
        tx.commit(name, &[], None).expect("commit");
    }
    // An update and a delete, to exercise churn.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "db1"),
            &record(&[("region", Value::String("ap".into()))]),
        )
        .expect("n");
        tx.delete_node(&node("Host", "cache1")).expect("del");
        tx.commit("churn", &[], None).expect("commit");
    }

    let root_before = repo
        .workspace_snapshot()
        .expect("snap")
        .manifest()
        .indexes
        .get("host_region")
        .copied();
    assert!(root_before.is_some());

    // Reindexing from scratch must reproduce the identical root (Invariant #5).
    repo.reindex().expect("reindex");
    let root_after = repo
        .workspace_snapshot()
        .expect("snap")
        .manifest()
        .indexes
        .get("host_region")
        .copied();
    assert_eq!(root_before, root_after, "reindex changed the index root");
}

#[test]
fn composite_index_maintains_a_value_tuple_and_is_null_blind() {
    // A composite (multi-property) index over (os, dc): its key is the ordered
    // value tuple; a node missing any component contributes no entry
    // (composite null-blind). ADR-0024 ratification / ADR-0027.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        for (k, os, dc) in [
            ("web1", "linux", Some("ams")),
            ("web2", "linux", Some("lon")),
            ("db1", "bsd", Some("ams")),
            ("partial", "linux", None), // missing dc → not indexed
        ] {
            let mut props = vec![("os", Value::String(os.into()))];
            if let Some(dc) = dc {
                props.push(("dc", Value::String(dc.into())));
            }
            tx.put_node(&node("Host", k), &record(&props)).expect("n");
        }
        tx.commit("nodes", &[], None).expect("commit");
    }
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Index {
            name: "host_os_dc".into(),
            def: IndexDef::new("Host", vec!["os".into(), "dc".into()]).expect("index"),
        })
        .expect("schema");
        tx.commit("declare composite index", &[], None)
            .expect("commit");
    }

    // The three fully-populated nodes are indexed under their (os, dc) tuple;
    // `partial` (no dc) is null-blind and absent.
    let snap = repo.workspace_snapshot().expect("snap");
    let mut got: Vec<(Vec<Value>, String)> = snap
        .index_entries("host_os_dc")
        .expect("entries")
        .into_iter()
        .map(|e| {
            let node = match &e.node().key()[0] {
                Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            };
            (e.values().to_vec(), node)
        })
        .collect();
    got.sort_by(|a, b| a.1.cmp(&b.1));
    let s = |x: &str| Value::String(x.into());
    assert_eq!(
        got,
        vec![
            (vec![s("bsd"), s("ams")], "db1".to_string()),
            (vec![s("linux"), s("ams")], "web1".to_string()),
            (vec![s("linux"), s("lon")], "web2".to_string()),
        ]
    );
    drop(snap);

    // Invariant #5: reindex reproduces the identical composite root.
    let root_before = repo
        .workspace_snapshot()
        .expect("snap")
        .manifest()
        .indexes
        .get("host_os_dc")
        .copied();
    repo.reindex().expect("reindex");
    let root_after = repo
        .workspace_snapshot()
        .expect("snap")
        .manifest()
        .indexes
        .get("host_os_dc")
        .copied();
    assert_eq!(
        root_before, root_after,
        "reindex changed the composite root"
    );
    assert!(root_before.is_some());

    // fsck (which shares index_entry_key) verifies the composite index clean.
    let report = fsck::check(&repo).expect("fsck");
    assert!(
        !report
            .findings
            .iter()
            .any(|f| matches!(f.kind, FindingKind::IndexInconsistency)),
        "composite index must be fsck-consistent: {:?}",
        report.findings
    );
}

#[test]
fn fsck_passes_a_maintained_index_and_flags_a_stale_one() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    declare_region_index(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "web1"),
            &record(&[("region", Value::String("eu".into()))]),
        )
        .expect("n");
        tx.commit("node", &[], None).expect("commit");
    }

    // A consistently-maintained index: no IndexInconsistency finding.
    let report = fsck::check(&repo).expect("fsck");
    assert!(
        !report
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::IndexInconsistency),
        "clean index tripped fsck: {:?}",
        report.findings
    );

    // Hand-build a manifest whose index map is stale (emptied), bypassing the
    // Transaction that keeps it consistent, and expose it as a workspace ref.
    let store = repo.store();
    let base = repo.workspace_manifest().expect("manifest");
    let empty = acetone_prolly::empty(store, base.chunk_params).expect("empty");
    let mut stale = base.clone();
    stale
        .indexes
        .insert("host_region".into(), MapRoot::from_root(&empty));
    let blob = store.put(&stale.encode()).expect("put manifest");
    store
        .write_ref("refs/acetone/workspaces/stale", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    let advisories: Vec<_> = report.advisories().collect();
    assert!(
        advisories
            .iter()
            .any(|f| f.kind == FindingKind::IndexInconsistency),
        "expected IndexInconsistency advisory, got {:?}",
        report.findings
    );
}

#[test]
fn multiple_ops_on_one_key_in_a_transaction_settle_on_the_final_state() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    declare_region_index(&repo);

    // Two puts of the same key in one transaction: the last wins.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "x"),
            &record(&[("region", Value::String("eu".into()))]),
        )
        .expect("n");
        tx.put_node(
            &node("Host", "x"),
            &record(&[("region", Value::String("us".into()))]),
        )
        .expect("n");
        tx.commit("double put", &[], None).expect("commit");
    }
    assert_eq!(
        index_contents(&repo, "host_region"),
        vec![(Value::String("us".into()), "x".into())]
    );

    // Put-then-delete of the same key in one transaction: no entry.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "y"),
            &record(&[("region", Value::String("ap".into()))]),
        )
        .expect("n");
        tx.delete_node(&node("Host", "y")).expect("del");
        tx.commit("put then delete", &[], None).expect("commit");
    }
    // Only x (us) remains; y never reaches the index.
    assert_eq!(
        index_contents(&repo, "host_region"),
        vec![(Value::String("us".into()), "x".into())]
    );
    repo.reindex().expect("reindex");
    assert_eq!(
        index_contents(&repo, "host_region"),
        vec![(Value::String("us".into()), "x".into())]
    );
}

#[test]
fn secondary_label_membership_is_indexed_and_maintained() {
    // Index on label `Tagged`, worn as a *secondary* label by a `Host` node.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Index {
            name: "tagged_region".into(),
            def: IndexDef::new("Tagged", vec!["region".into()]).expect("idx"),
        })
        .expect("s");
        tx.commit("index", &[], None).expect("commit");
    }

    // A Host that also bears the secondary label Tagged is indexed.
    let tagged = NodeRecord::new(
        ["Tagged".to_owned()],
        BTreeMap::from([("region".to_owned(), Value::String("eu".into()))]),
    );
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(&node("Host", "web1"), &tagged).expect("n");
        tx.commit("tagged host", &[], None).expect("commit");
    }
    assert_eq!(
        index_contents(&repo, "tagged_region"),
        vec![(Value::String("eu".into()), "web1".into())]
    );

    // Dropping the secondary label removes the entry.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "web1"),
            &record(&[("region", Value::String("eu".into()))]),
        )
        .expect("n");
        tx.commit("untag", &[], None).expect("commit");
    }
    assert!(index_contents(&repo, "tagged_region").is_empty());
}

#[test]
fn fsck_flags_an_index_map_with_no_schema_declaration() {
    // A `idx/<name>` map present with no declaring schema entry is stale.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(&node("Host", "web1"), &record(&[])).expect("n");
        tx.commit("node", &[], None).expect("commit");
    }

    let store = repo.store();
    let base = repo.workspace_manifest().expect("manifest");
    let empty = acetone_prolly::empty(store, base.chunk_params).expect("empty");
    let mut ghost = base.clone();
    ghost
        .indexes
        .insert("ghost".into(), MapRoot::from_root(&empty));
    let blob = store.put(&ghost.encode()).expect("put");
    store
        .write_ref("refs/acetone/workspaces/ghost", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report
            .advisories()
            .any(|f| f.kind == FindingKind::IndexInconsistency),
        "expected IndexInconsistency for a schema-less index map: {:?}",
        report.findings
    );
}

#[test]
fn fsck_flags_a_declared_index_with_no_map() {
    // The mirror: a schema-declared index whose map is entirely absent.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    declare_region_index(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(
            &node("Host", "web1"),
            &record(&[("region", Value::String("eu".into()))]),
        )
        .expect("n");
        tx.commit("node", &[], None).expect("commit");
    }

    let store = repo.store();
    let mut base = repo.workspace_manifest().expect("manifest");
    base.indexes.remove("host_region"); // schema still declares it
    let blob = store.put(&base.encode()).expect("put");
    store
        .write_ref("refs/acetone/workspaces/nomap", None, &blob)
        .expect("ref");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report
            .advisories()
            .any(|f| f.kind == FindingKind::IndexInconsistency),
        "expected IndexInconsistency for a declared index with no map: {:?}",
        report.findings
    );
}

#[test]
fn key_property_index_sources_value_from_the_node_key() {
    // Indexing a KEY property must read the value from the node key (records
    // exclude key properties, Invariant #3).
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Index {
            name: "host_name".into(),
            def: IndexDef::new("Host", vec!["name".into()]).expect("idx"),
        })
        .expect("s");
        tx.commit("index on key", &[], None).expect("commit");
    }
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(&node("Host", "web1"), &record(&[])).expect("n");
        tx.commit("node", &[], None).expect("commit");
    }
    assert_eq!(
        index_contents(&repo, "host_name"),
        vec![(Value::String("web1".into()), "web1".into())]
    );
}

#[test]
fn redeclaring_an_index_with_a_new_property_rebuilds_it() {
    // U8 (pre-0.1 review): redeclaring an index under the same name but a
    // different property must rebuild its map. An incremental delta over the map
    // built for the old property leaves stale entries, so seeks on the new
    // property return wrong (usually empty) results.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    declare_region_index(&repo); // "host_region" over property `region`

    let mut tx = repo.begin_write().expect("begin");
    tx.put_node(
        &node("Host", "h1"),
        &record(&[
            ("region", Value::String("eu".into())),
            ("zone", Value::String("z1".into())),
        ]),
    )
    .expect("put");
    tx.commit("add host", &[], None).expect("commit");

    // Baseline: the index is over `region`.
    let before: Vec<Value> = index_contents(&repo, "host_region")
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    assert_eq!(before, vec![Value::String("eu".into())]);

    // Redeclare the same index over `zone`.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Index {
        name: "host_region".into(),
        def: IndexDef::new("Host", vec!["zone".into()]).expect("index"),
    })
    .expect("schema");
    tx.commit("redeclare index on zone", &[], None)
        .expect("commit");

    // The index now reflects `zone`, not the stale `region`.
    let after: Vec<Value> = index_contents(&repo, "host_region")
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    assert_eq!(
        after,
        vec![Value::String("z1".into())],
        "index must be rebuilt over the new property, not left stale on the old one"
    );
    // And it is consistent with `nodes` under the new schema (no stale advisory).
    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.is_clean(),
        "redeclared index must be consistent with nodes: {:?}",
        report.findings
    );
}

#[test]
fn redeclaring_a_composite_index_with_reordered_properties_rebuilds_it() {
    // U8 follow-up (ADR-0027): a composite index's property *order* is
    // significant (it determines the key tuple), so reordering it is a
    // definition change that must rebuild — not an incremental no-op.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Index {
        name: "hz".into(),
        def: IndexDef::new("Host", vec!["region".into(), "zone".into()]).expect("index"),
    })
    .expect("schema");
    tx.put_node(
        &node("Host", "h1"),
        &record(&[
            ("region", Value::String("eu".into())),
            ("zone", Value::String("z1".into())),
        ]),
    )
    .expect("put");
    tx.commit("declare + node", &[], None).expect("commit");

    // Redeclare the same index with the properties reordered.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Index {
        name: "hz".into(),
        def: IndexDef::new("Host", vec!["zone".into(), "region".into()]).expect("index"),
    })
    .expect("schema");
    tx.commit("reorder composite index", &[], None)
        .expect("commit");

    let report = fsck::check(&repo).expect("fsck");
    assert!(
        report.is_clean(),
        "a reordered composite index must be rebuilt to match nodes: {:?}",
        report.findings
    );
}

#[test]
fn redeclaring_one_of_two_indexes_leaves_the_other_untouched() {
    // U8 follow-up: only the redefined index rebuilds; an unchanged sibling
    // stays cheap-incremental and correct.
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Index {
        name: "a_idx".into(),
        def: IndexDef::new("Host", vec!["region".into()]).expect("index"),
    })
    .expect("schema");
    tx.put_schema(&SchemaEntry::Index {
        name: "b_idx".into(),
        def: IndexDef::new("Host", vec!["zone".into()]).expect("index"),
    })
    .expect("schema");
    tx.put_node(
        &node("Host", "h1"),
        &record(&[
            ("region", Value::String("eu".into())),
            ("zone", Value::String("z1".into())),
            ("tier", Value::String("t1".into())),
        ]),
    )
    .expect("put");
    tx.commit("two indexes + node", &[], None).expect("commit");

    // Redeclare only a_idx, moving it from `region` to `tier`.
    let mut tx = repo.begin_write().expect("begin");
    tx.put_schema(&SchemaEntry::Index {
        name: "a_idx".into(),
        def: IndexDef::new("Host", vec!["tier".into()]).expect("index"),
    })
    .expect("schema");
    tx.commit("redeclare a_idx on tier", &[], None)
        .expect("commit");

    let a: Vec<Value> = index_contents(&repo, "a_idx")
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    let b: Vec<Value> = index_contents(&repo, "b_idx")
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    assert_eq!(
        a,
        vec![Value::String("t1".into())],
        "a_idx rebuilt over `tier`"
    );
    assert_eq!(
        b,
        vec![Value::String("z1".into())],
        "b_idx unchanged over `zone`"
    );
    assert!(fsck::check(&repo).expect("fsck").is_clean());
}

#[test]
fn index_scan_returns_the_node_keys_for_an_equality_prefix() {
    // The lazy store-backed IndexSeek primitive (ADR-0040): a value prefix
    // selects exactly the matching node keys, agreeing with a full index scan.
    use acetone_model::graph_keys::index_value_prefix;

    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    declare_region_index(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        for (host, region) in [("web1", "eu"), ("web2", "eu"), ("db1", "us")] {
            tx.put_node(
                &node("Host", host),
                &record(&[("region", Value::String(region.into()))]),
            )
            .expect("n");
        }
        tx.commit("nodes", &[], None).expect("commit");
    }

    let snap = repo.workspace_snapshot().expect("snap");
    let prefix = index_value_prefix("Host", &["region".into()], &[Value::String("eu".into())])
        .expect("prefix");
    let mut got: Vec<String> = snap
        .index_scan("host_region", &prefix)
        .expect("scan")
        .expect("index present")
        .into_iter()
        .map(|k| match &k.key()[0] {
            Value::String(s) => s.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    got.sort();
    assert_eq!(got, vec!["web1".to_string(), "web2".to_string()]);

    // A value with no entries yields an empty (not absent) result.
    let none = index_value_prefix("Host", &["region".into()], &[Value::String("ap".into())])
        .expect("prefix");
    assert!(
        snap.index_scan("host_region", &none)
            .expect("scan")
            .expect("present")
            .is_empty()
    );

    // An undeclared index is absent, so the caller can fall back to a scan.
    assert!(
        snap.index_scan("no_such_index", &prefix)
            .expect("scan")
            .is_none()
    );
}

#[test]
fn out_and_in_edges_are_degree_bounded_and_agree_with_a_full_scan() {
    use acetone_model::graph_keys::EdgeKey;
    use acetone_model::records::EdgeRecord;

    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());
    declare_host_label(&repo);
    {
        let mut tx = repo.begin_write().expect("begin");
        for host in ["a", "b", "c"] {
            tx.put_node(&node("Host", host), &record(&[])).expect("n");
        }
        // a -> b, a -> c, b -> c.
        for (src, dst) in [("a", "b"), ("a", "c"), ("b", "c")] {
            tx.put_edge(
                &EdgeKey::new(node("Host", src), "LINK", node("Host", dst), Value::Null)
                    .expect("edge"),
                &EdgeRecord::new(std::collections::BTreeMap::new()),
            )
            .expect("edge");
        }
        tx.commit("graph", &[], None).expect("commit");
    }

    let snap = repo.workspace_snapshot().expect("snap");

    // out_edges(a) = {a->b, a->c}; matches the forward map filtered to src == a.
    let mut out: Vec<String> = snap
        .out_edges(&node("Host", "a"))
        .expect("out")
        .into_iter()
        .map(|(k, _)| dst_of(&k))
        .collect();
    out.sort();
    assert_eq!(out, vec!["b".to_string(), "c".to_string()]);

    // in_edges(c) = {a->c, b->c}; the reverse map keyed by dst.
    let mut into: Vec<String> = snap
        .in_edges(&node("Host", "c"))
        .expect("in")
        .into_iter()
        .map(|(k, _)| src_of(&k))
        .collect();
    into.sort();
    assert_eq!(into, vec!["a".to_string(), "b".to_string()]);

    // A node with no out-edges reads back empty (degree-bounded, no whole-map load).
    assert!(snap.out_edges(&node("Host", "c")).expect("out").is_empty());
}

fn dst_of(key: &acetone_model::graph_keys::EdgeKey) -> String {
    match &key.dst().key()[0] {
        Value::String(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn src_of(key: &acetone_model::graph_keys::EdgeKey) -> String {
    match &key.src().key()[0] {
        Value::String(s) => s.clone(),
        other => format!("{other:?}"),
    }
}
