//! Integration test for `gc` (spec §3.1, §7, ADR-0011): after churn, `gc`
//! consolidates the loose objects into a packfile (deltaing rewritten chunks
//! against their write-time predecessors) and reclaims space.

use std::path::Path;

use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{LabelDef, SchemaEntry};
use std::collections::BTreeMap;

fn init_repo(dir: &Path) -> Repository {
    Repository::init(&dir.join("graph.git"), InitOptions::default()).expect("init")
}

/// Total size of every file under `dir`, recursively.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        let meta = std::fs::symlink_metadata(&path).expect("meta");
        if meta.is_dir() {
            total += dir_size(&path);
        } else {
            total += meta.len();
        }
    }
    total
}

#[test]
fn gc_reclaims_space_after_churn() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = init_repo(dir.path());

    // Declare a label and seed a body of nodes so the maps are multi-level.
    {
        let mut tx = repo.begin_write().expect("begin");
        tx.put_schema(&SchemaEntry::Label {
            name: "N".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
        })
        .expect("schema");
        for i in 0..500 {
            let key = NodeKey::new("N", vec![Value::Int(i)]).expect("k");
            tx.put_node(
                &key,
                &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(0))])),
            )
            .expect("n");
        }
        tx.commit("seed", &[], None).expect("commit");
    }

    // Churn: many commits, each rewriting one node's value — every commit
    // rewrites the leaf (and interior) chunks on its root-to-leaf path,
    // producing loose objects that mostly differ by a little from predecessors.
    for round in 1..=80i64 {
        let mut tx = repo.begin_write().expect("begin");
        let key = NodeKey::new("N", vec![Value::Int(round % 500)]).expect("k");
        tx.put_node(
            &key,
            &NodeRecord::new([], BTreeMap::from([("v".to_owned(), Value::Int(round))])),
        )
        .expect("n");
        tx.commit(&format!("churn {round}"), &[], None)
            .expect("commit");
    }

    let git_dir = repo.store().git_dir().to_path_buf();
    let before = dir_size(&git_dir);

    let stats = repo.gc().expect("gc");
    assert!(stats.objects > 0, "gc packed nothing");
    // The write path recorded base hints, so churn-rewritten chunks delta
    // against their predecessors rather than being stored whole (ADR-0011).
    assert!(stats.deltas > 0, "no deltas — base hints were not recorded");

    let after = dir_size(&git_dir);
    assert!(
        after < before,
        "gc did not reclaim space: {before} → {after} bytes ({} objects, {} deltas)",
        stats.objects,
        stats.deltas
    );

    // The repository is still fully readable and intact after consolidation.
    let report = acetone_graph::fsck(&repo).expect("fsck");
    assert!(
        !report.has_errors(),
        "fsck errors after gc: {:?}",
        report.findings
    );
    let snapshot = repo.workspace_snapshot().expect("snap");
    assert_eq!(snapshot.nodes().expect("nodes").len(), 500);
}
