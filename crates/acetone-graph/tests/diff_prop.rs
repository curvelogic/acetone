//! Property test: `Repository::diff` between two committed versions equals
//! the independently-computed node-set difference — for any pair of random
//! versions (spec §7, acetone-14c.1). Complements the worked example in
//! `repository.rs::diff_classifies_node_and_edge_changes`.

use acetone_graph::diff::ChangeKind;
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::NodeRecord;
use proptest::prelude::*;
use std::collections::BTreeMap;

/// A version is a small map from node id to a property value. Two versions
/// exercise every change kind: an id in one only (Added/Removed), an id in
/// both with a different value (Modified), or the same value (unchanged).
fn version() -> impl Strategy<Value = BTreeMap<u8, i64>> {
    proptest::collection::btree_map(0u8..8, 0i64..4, 0..8)
}

fn node(id: u8) -> NodeKey {
    NodeKey::new("N", vec![Value::Int(i64::from(id))]).expect("valid key")
}

fn record(v: i64) -> NodeRecord {
    NodeRecord::new([], BTreeMap::from([("v".to_string(), Value::Int(v))]))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn diff_equals_the_model_node_difference(a in version(), b in version()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default())
            .expect("init");

        // v1 = version a.
        let mut tx = repo.begin_write().expect("begin");
        for (id, v) in &a {
            tx.put_node(&node(*id), &record(*v)).expect("put");
        }
        let v1 = tx.commit("a", &[], None).expect("commit a");

        // v2 = version b: delete the ids that are gone, upsert the rest.
        let mut tx = repo.begin_write().expect("begin");
        for id in a.keys() {
            if !b.contains_key(id) {
                tx.delete_node(&node(*id)).expect("delete");
            }
        }
        for (id, v) in &b {
            tx.put_node(&node(*id), &record(*v)).expect("put");
        }
        let v2 = tx.commit("b", &[], None).expect("commit b");

        // The model difference, keyed by encoded node key.
        let mut want: BTreeMap<Vec<u8>, ChangeKind> = BTreeMap::new();
        for id in a.keys().chain(b.keys()) {
            let kind = match (a.get(id), b.get(id)) {
                (Some(_), None) => ChangeKind::Removed,
                (None, Some(_)) => ChangeKind::Added,
                (Some(x), Some(y)) if x != y => ChangeKind::Modified,
                _ => continue, // unchanged
            };
            want.insert(node(*id).encode().expect("encode"), kind);
        }

        let got: BTreeMap<Vec<u8>, ChangeKind> = repo
            .diff(&v1.to_hex(), &v2.to_hex())
            .expect("diff")
            .nodes
            .iter()
            .map(|n| (n.key.encode().expect("encode"), n.kind))
            .collect();

        prop_assert_eq!(got, want);
    }
}
