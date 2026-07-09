//! Property test for Load-Bearing Invariant #5 (derived maps): a declared
//! index maintained incrementally through an arbitrary sequence of node
//! upserts and deletes MUST have the exact same map root as one rebuilt from
//! scratch by `reindex` (spec §3.3: "reindex MUST reproduce identical roots").

use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
use proptest::prelude::*;
use std::collections::BTreeMap;

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

/// A record with an indexed `region` property, or absent/null variants, so the
/// null-blind rule and the delete path are both exercised.
fn record(region: Option<Option<u8>>) -> NodeRecord {
    let mut props = BTreeMap::new();
    match region {
        Some(Some(r)) => {
            props.insert("region".to_string(), Value::String(format!("r{r}")));
        }
        Some(None) => {
            props.insert("region".to_string(), Value::Null);
        }
        None => {}
    }
    NodeRecord::new([], props)
}

/// One step: upsert id with a region variant, or delete id.
type Step = (u8, Option<Option<Option<u8>>>);

fn steps() -> impl Strategy<Value = Vec<Step>> {
    let region = prop::option::of(prop::option::of(0u8..3)); // Some(region-variant) = upsert
    let step = (0u8..5, prop::option::of(region)); // outer None = delete
    proptest::collection::vec(step, 0..12)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn incremental_index_matches_reindex(steps in steps()) {
        let dir = tempfile::tempdir().expect("tmp");
        let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default())
            .expect("init");

        // Declare label N(key id) and an index on N.region.
        {
            let mut tx = repo.begin_write().expect("begin");
            tx.put_schema(&SchemaEntry::Label {
                name: "N".into(),
                def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
            }).expect("s");
            tx.put_schema(&SchemaEntry::Index {
                name: "n_region".into(),
                def: IndexDef::new("N", vec!["region".into()]).expect("idx"),
            }).expect("s");
            tx.commit("schema", &[], None).expect("commit");
        }

        // Apply the steps, each its own commit, maintained incrementally.
        for (id, op) in &steps {
            let mut tx = repo.begin_write().expect("begin");
            match op {
                Some(region) => tx.put_node(&node(*id), &record(*region)).expect("put"),
                None => tx.delete_node(&node(*id)).expect("del"),
            }
            // A delete of an absent key is a harmless no-op tombstone; commit
            // may be a no-op, which is fine.
            let _ = tx.commit("step", &[], None);
        }

        let incremental = repo.workspace_snapshot().expect("snap")
            .manifest().indexes.get("n_region").copied();

        repo.reindex().expect("reindex");
        let rebuilt = repo.workspace_snapshot().expect("snap")
            .manifest().indexes.get("n_region").copied();

        prop_assert_eq!(incremental, rebuilt);
    }
}
