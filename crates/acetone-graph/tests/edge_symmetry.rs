//! Property test: `edges_fwd` and `edges_rev` carry exactly the same
//! edge set after any sequence of put/delete mutations across any number
//! of transactions (spec §3.3: edges_rev MUST be maintained
//! transactionally with edges_fwd).

use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::EdgeRecord;
use proptest::prelude::*;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
enum Op {
    Put(u8, u8, u8),    // src id, type id, dst id
    Delete(u8, u8, u8), // ditto
    SaveBoundary,       // close the transaction and open a new one
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..6, 0u8..3, 0u8..6).prop_map(|(s, t, d)| Op::Put(s, t, d)),
        2 => (0u8..6, 0u8..3, 0u8..6).prop_map(|(s, t, d)| Op::Delete(s, t, d)),
        1 => Just(Op::SaveBoundary),
    ]
}

fn edge_key(s: u8, t: u8, d: u8) -> EdgeKey {
    let node = |i: u8| NodeKey::new("N", vec![Value::Int(i64::from(i))]).expect("valid");
    EdgeKey::new(node(s), format!("T{t}"), node(d), Value::Null).expect("valid")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn edge_maps_stay_symmetric(ops in proptest::collection::vec(op(), 1..40)) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(&dir.path().join("g.git"), InitOptions::default())
            .expect("init");

        // Model of the expected edge set, keyed like the graph is.
        let mut expected: BTreeMap<Vec<u8>, EdgeKey> = BTreeMap::new();

        let mut tx = Some(repo.begin_write().expect("begin"));
        for step in &ops {
            match step {
                Op::Put(s, t, d) => {
                    let key = edge_key(*s, *t, *d);
                    tx.as_mut().unwrap().put_edge(&key, &EdgeRecord::default()).expect("put");
                    expected.insert(key.encode_fwd().expect("encode"), key);
                }
                Op::Delete(s, t, d) => {
                    let key = edge_key(*s, *t, *d);
                    tx.as_mut().unwrap().delete_edge(&key).expect("delete");
                    expected.remove(&key.encode_fwd().expect("encode"));
                }
                Op::SaveBoundary => {
                    tx.take().unwrap().save().expect("save");
                    tx = Some(repo.begin_write().expect("begin"));
                }
            }
        }
        tx.take().unwrap().save().expect("final save");

        let snapshot = repo.workspace_snapshot().expect("snapshot");
        let fwd: Vec<EdgeKey> = snapshot.edges().expect("edges").into_iter().map(|(k, _)| k).collect();
        let rev = snapshot.reverse_edge_keys().expect("rev");
        let want: Vec<EdgeKey> = expected.into_values().collect();

        prop_assert_eq!(&fwd, &want, "forward map must equal the model");
        // Same set, possibly different order (reverse map sorts by dst).
        let mut rev_sorted: Vec<Vec<u8>> =
            rev.iter().map(|k| k.encode_fwd().expect("encode")).collect();
        rev_sorted.sort();
        let mut fwd_sorted: Vec<Vec<u8>> =
            fwd.iter().map(|k| k.encode_fwd().expect("encode")).collect();
        fwd_sorted.sort();
        prop_assert_eq!(rev_sorted, fwd_sorted, "reverse map must mirror forward map");
    }
}
