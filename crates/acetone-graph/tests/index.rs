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
        def: IndexDef::new("Host", "region").expect("index"),
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
            (e.value().clone(), node)
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
            def: IndexDef::new("M", "score").expect("idx"),
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
            def: IndexDef::new("Host", "name").expect("idx"),
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
