//! Anchor-set export: `reachable_chunks` must enumerate the **complete**
//! transitive chunk set of a root (the `NewCommit::anchors` contract —
//! anything missed is pruned by `git gc` and absent from clones), and the
//! accumulator form must make cross-version reuse cheap.

mod common;

use std::collections::BTreeSet;

use acetone_prolly::{
    BatchOp, ChunkParams, apply_batch, bulk_load, collect_reachable_chunks, empty, reachable_chunks,
};
use common::{MemStore, bulk_entries};

#[test]
fn reachable_set_is_exactly_the_written_set() {
    // A store that has held exactly one tree: the walk must return every
    // chunk the build wrote — no more, no less.
    let store = MemStore::new();
    let map = bulk_entries(20_000, 0xa11);
    let root = bulk_load(&store, ChunkParams::default(), map).expect("bulk_load");
    assert!(root.height() >= 3);

    let walked = reachable_chunks(&store, &root).expect("walk");
    assert_eq!(
        walked,
        store.all_hashes(),
        "walk must equal the written set"
    );
}

#[test]
fn walk_reads_only_internal_nodes() {
    let store = MemStore::new();
    let map = bulk_entries(20_000, 0xa12);
    let root = bulk_load(&store, ChunkParams::default(), map).expect("bulk_load");
    let total = store.len() as u64;

    store.reset_counters();
    let walked = reachable_chunks(&store, &root).expect("walk");
    let internal_reads = store.reads();
    assert_eq!(walked.len() as u64, total);
    assert!(
        internal_reads < total / 2,
        "walk read {internal_reads} chunks; leaves ({} of {total}) must not be read",
        total - internal_reads
    );
}

#[test]
fn empty_tree_has_exactly_one_chunk() {
    let store = MemStore::new();
    let root = empty(&store, ChunkParams::default()).expect("empty");
    let walked = reachable_chunks(&store, &root).expect("walk");
    assert_eq!(walked, vec![root.hash()]);
}

#[test]
fn accumulator_reuse_skips_shared_subtrees() {
    let store = MemStore::new();
    let map = bulk_entries(20_000, 0xa13);
    let v1 = bulk_load(&store, ChunkParams::default(), map.clone()).expect("bulk_load");
    let key = map.keys().nth(10_000).expect("key").clone();
    let v2 = apply_batch(&store, &v1, vec![BatchOp::Put(key, b"changed".to_vec())])
        .expect("apply_batch");

    // Full walk of v1 into the accumulator.
    let mut anchors = BTreeSet::new();
    collect_reachable_chunks(&store, &v1, &mut anchors).expect("walk v1");
    let v1_count = anchors.len();

    // Adding v2 must read only v2's new spine, not re-walk the shared bulk.
    store.reset_counters();
    collect_reachable_chunks(&store, &v2, &mut anchors).expect("walk v2");
    let extra_reads = store.reads();
    let height = u64::from(v2.height());
    assert!(
        extra_reads <= 2 * height + 2,
        "incremental walk read {extra_reads} chunks for a single-key change"
    );
    // The union covers both versions completely.
    let mut expected = BTreeSet::new();
    collect_reachable_chunks(&store, &v2, &mut BTreeSet::new()).expect("fresh v2 walk");
    for h in reachable_chunks(&store, &v1).expect("v1") {
        expected.insert(h);
    }
    for h in reachable_chunks(&store, &v2).expect("v2") {
        expected.insert(h);
    }
    assert_eq!(anchors, expected);
    assert!(anchors.len() > v1_count, "v2 must contribute new chunks");
}
