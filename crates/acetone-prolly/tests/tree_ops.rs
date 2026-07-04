//! Behavioural tests for build, point get, forward/reverse scans and
//! batched mutation, against the in-memory store.

mod common;

use std::ops::Bound;

use acetone_prolly::{
    BatchOp, ChunkParams, Root, apply_batch, bulk_load, empty, get, scan, scan_rev,
};
use common::{Map, MemStore, bulk_entries};

fn build(store: &MemStore, map: &Map) -> Root {
    bulk_load(store, ChunkParams::default(), map.clone()).expect("bulk_load")
}

fn scan_all(store: &MemStore, root: &Root) -> Vec<(Vec<u8>, Vec<u8>)> {
    scan(store, root, ..)
        .expect("scan")
        .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
        .collect::<Result<_, _>>()
        .expect("scan items")
}

#[test]
fn empty_map_round_trips() {
    let store = MemStore::new();
    let root = empty(&store, ChunkParams::default()).expect("empty");
    assert_eq!(root.height(), 1);
    assert_eq!(get(&store, &root, b"anything").expect("get"), None);
    assert!(scan_all(&store, &root).is_empty());
    assert_eq!(scan_rev(&store, &root, ..).expect("scan_rev").count(), 0);
}

#[test]
fn small_map_get_and_scan() {
    let store = MemStore::new();
    let mut map = Map::new();
    for i in 0..100u32 {
        map.insert(
            format!("key/{i:04}").into_bytes(),
            format!("value-{i}").into_bytes(),
        );
    }
    let root = build(&store, &map);
    assert_eq!(root.height(), 1, "100 small entries fit one leaf");

    for (k, v) in &map {
        assert_eq!(
            get(&store, &root, k).expect("get").as_deref(),
            Some(v.as_slice())
        );
    }
    assert_eq!(get(&store, &root, b"absent").expect("get"), None);
    assert_eq!(
        scan_all(&store, &root),
        map.clone().into_iter().collect::<Vec<_>>()
    );
}

#[test]
fn large_map_spans_multiple_levels() {
    let store = MemStore::new();
    let map = bulk_entries(20_000, 0xace7);
    let root = build(&store, &map);
    assert!(
        root.height() >= 3,
        "20k entries should build height >= 3, got {}",
        root.height()
    );

    // Point gets across the whole key space.
    for (k, v) in map.iter().step_by(97) {
        assert_eq!(
            get(&store, &root, k).expect("get").as_deref(),
            Some(v.as_slice())
        );
    }
    // Full scan equals the reference map.
    assert_eq!(scan_all(&store, &root), map.into_iter().collect::<Vec<_>>());
}

#[test]
fn point_get_touches_only_one_path() {
    let store = MemStore::new();
    let map = bulk_entries(5000, 1);
    let root = build(&store, &map);
    let key = map.keys().nth(2500).expect("key").clone();

    store.reset_counters();
    get(&store, &root, &key).expect("get").expect("present");
    assert_eq!(
        store.reads(),
        u64::from(root.height()),
        "a point get reads exactly one chunk per level"
    );
}

#[test]
fn range_scans_match_btreemap_semantics() {
    let store = MemStore::new();
    let map = bulk_entries(2000, 7);
    let root = build(&store, &map);

    let keys: Vec<&Vec<u8>> = map.keys().collect();
    let lo = keys[321].clone();
    let hi = keys[1234].clone();

    type BoundPair = (Bound<Vec<u8>>, Bound<Vec<u8>>);
    let cases: Vec<BoundPair> = vec![
        (Bound::Included(lo.clone()), Bound::Excluded(hi.clone())),
        (Bound::Included(lo.clone()), Bound::Included(hi.clone())),
        (Bound::Excluded(lo.clone()), Bound::Included(hi.clone())),
        (Bound::Excluded(lo.clone()), Bound::Excluded(hi.clone())),
        (Bound::Unbounded, Bound::Included(hi.clone())),
        (Bound::Included(lo.clone()), Bound::Unbounded),
        (Bound::Unbounded, Bound::Unbounded),
        // Empty range (BTreeMap-legal: start == end).
        (Bound::Included(hi.clone()), Bound::Excluded(hi.clone())),
    ];
    for (start, end) in cases {
        let expected: Vec<(Vec<u8>, Vec<u8>)> = map
            .range::<Vec<u8>, _>((start.clone(), end.clone()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let range = (
            match &start {
                Bound::Included(k) => Bound::Included(k.as_slice()),
                Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
                Bound::Unbounded => Bound::Unbounded,
            },
            match &end {
                Bound::Included(k) => Bound::Included(k.as_slice()),
                Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
                Bound::Unbounded => Bound::Unbounded,
            },
        );
        let got: Vec<(Vec<u8>, Vec<u8>)> = scan(&store, &root, range)
            .expect("scan")
            .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
            .collect::<Result<_, _>>()
            .expect("scan items");
        assert_eq!(got, expected, "forward scan {start:?}..{end:?}");

        let mut rev: Vec<(Vec<u8>, Vec<u8>)> = scan_rev(&store, &root, range)
            .expect("scan_rev")
            .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
            .collect::<Result<_, _>>()
            .expect("scan_rev items");
        rev.reverse();
        assert_eq!(rev, expected, "reverse scan {start:?}..{end:?}");
    }

    // An inverted range (start above end) yields nothing rather than
    // panicking (BTreeMap panics here; scans are total instead).
    let inverted = (
        Bound::Excluded(hi.as_slice()),
        Bound::Excluded(lo.as_slice()),
    );
    assert_eq!(scan(&store, &root, inverted).expect("scan").count(), 0);
    assert_eq!(
        scan_rev(&store, &root, inverted).expect("scan_rev").count(),
        0
    );
}

#[test]
fn scan_bounds_between_keys_and_outside_key_space() {
    let store = MemStore::new();
    let map = bulk_entries(500, 9);
    let root = build(&store, &map);

    // Bounds that are not keys themselves.
    let probes: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"aaaa".to_vec(),                         // below every key
        b"bulk/0000000000000009/0000zz".to_vec(), // between keys
        b"zzzz".to_vec(),                         // above every key
    ];
    for p in probes {
        let expected: Vec<Vec<u8>> = map
            .range::<Vec<u8>, _>((Bound::Included(p.clone()), Bound::Unbounded))
            .map(|(k, _)| k.clone())
            .collect();
        let got: Vec<Vec<u8>> = scan(
            &store,
            &root,
            (Bound::Included(p.as_slice()), Bound::Unbounded),
        )
        .expect("scan")
        .map(|r| r.map(|(k, _)| k.to_vec()))
        .collect::<Result<_, _>>()
        .expect("items");
        assert_eq!(
            got,
            expected,
            "forward from {:?}",
            String::from_utf8_lossy(&p)
        );

        let expected_rev: Vec<Vec<u8>> = map
            .range::<Vec<u8>, _>((Bound::Unbounded, Bound::Excluded(p.clone())))
            .rev()
            .map(|(k, _)| k.clone())
            .collect();
        let got_rev: Vec<Vec<u8>> = scan_rev(
            &store,
            &root,
            (Bound::Unbounded, Bound::Excluded(p.as_slice())),
        )
        .expect("scan_rev")
        .map(|r| r.map(|(k, _)| k.to_vec()))
        .collect::<Result<_, _>>()
        .expect("items");
        assert_eq!(
            got_rev,
            expected_rev,
            "reverse to {:?}",
            String::from_utf8_lossy(&p)
        );
    }
}

#[test]
fn batch_apply_matches_bulk_load_of_result() {
    let store = MemStore::new();
    let mut map = bulk_entries(3000, 42);
    let root = build(&store, &map);

    // A mixed batch: overwrite, insert, delete, no-op delete.
    let mut ops: Vec<BatchOp> = Vec::new();
    for (i, k) in map.keys().cloned().enumerate().take(600) {
        if i % 3 == 0 {
            ops.push(BatchOp::Delete(k));
        } else if i % 3 == 1 {
            ops.push(BatchOp::Put(k, format!("updated-{i}").into_bytes()));
        }
    }
    ops.push(BatchOp::Put(b"new/key/1".to_vec(), b"fresh".to_vec()));
    ops.push(BatchOp::Put(b"zzzz/beyond".to_vec(), b"tail".to_vec()));
    ops.push(BatchOp::Delete(b"never/existed".to_vec()));

    for op in &ops {
        match op {
            BatchOp::Put(k, v) => {
                map.insert(k.clone(), v.clone());
            }
            BatchOp::Delete(k) => {
                map.remove(k);
            }
        }
    }

    let updated = apply_batch(&store, &root, ops).expect("apply_batch");
    let fresh = build(&store, &map);
    assert_eq!(
        updated, fresh,
        "splice result must be bit-identical to a fresh build"
    );
    assert_eq!(
        scan_all(&store, &updated),
        map.into_iter().collect::<Vec<_>>()
    );
}

#[test]
fn batch_loads_only_affected_paths() {
    let store = MemStore::new();
    let map = bulk_entries(5000, 0xfeed);
    let root = build(&store, &map);
    let total_chunks = store.len() as u64;
    let key = map.keys().nth(2500).expect("key").clone();

    store.reset_counters();
    let updated = apply_batch(&store, &root, vec![BatchOp::Put(key, b"changed".to_vec())])
        .expect("apply_batch");
    assert_ne!(updated, root);

    // The declared fix over the spike: a single-key update must read the
    // root→leaf path plus a bounded resynchronisation overhead — nowhere
    // near the whole internal node set, let alone all chunks.
    let reads = store.reads();
    let height = u64::from(root.height());
    assert!(
        reads <= 4 * height + 8,
        "single-key update read {reads} chunks (height {height}, {total_chunks} total)"
    );
}

#[test]
fn duplicate_ops_last_one_wins_and_empty_batch_is_identity() {
    let store = MemStore::new();
    let map = bulk_entries(200, 3);
    let root = build(&store, &map);

    assert_eq!(
        apply_batch(&store, &root, Vec::new()).expect("empty batch"),
        root
    );

    let k = map.keys().next().expect("key").clone();
    let updated = apply_batch(
        &store,
        &root,
        vec![
            BatchOp::Put(k.clone(), b"first".to_vec()),
            BatchOp::Delete(k.clone()),
            BatchOp::Put(k.clone(), b"final".to_vec()),
        ],
    )
    .expect("apply_batch");
    assert_eq!(
        get(&store, &updated, &k).expect("get").as_deref(),
        Some(b"final".as_slice())
    );
}

#[test]
fn everything_deleted_returns_canonical_empty_root() {
    let store = MemStore::new();
    let map = bulk_entries(2500, 11);
    let root = build(&store, &map);
    let emptied =
        apply_batch(&store, &root, map.keys().cloned().map(BatchOp::Delete)).expect("apply_batch");
    let canonical = empty(&store, ChunkParams::default()).expect("empty");
    assert_eq!(emptied, canonical);
}

#[test]
fn bulk_load_duplicate_keys_last_wins() {
    let store = MemStore::new();
    let entries = vec![
        (b"k".to_vec(), b"first".to_vec()),
        (b"other".to_vec(), b"x".to_vec()),
        (b"k".to_vec(), b"last".to_vec()),
    ];
    let root = bulk_load(&store, ChunkParams::default(), entries).expect("bulk_load");
    assert_eq!(
        get(&store, &root, b"k").expect("get").as_deref(),
        Some(b"last".as_slice())
    );
}

#[test]
fn oversized_entry_is_rejected_not_truncated() {
    // Keys/values near the u32 frame limit are unbuildable in a test, but
    // the store's object-size cap fires first for merely-huge entries and
    // must surface as a typed error, not a truncation or a panic.
    let store = MemStore::with_cap(1024 * 1024);
    let huge = vec![0u8; 2 * 1024 * 1024];
    let err = bulk_load(&store, ChunkParams::default(), vec![(b"k".to_vec(), huge)])
        .expect_err("oversized entry must fail");
    let msg = err.to_string();
    assert!(msg.contains("exceeds"), "expected a size error, got: {msg}");
}

// ---------------------------------------------------------------------------
// Property — random ranges, both directions, against a reference filter
// ---------------------------------------------------------------------------

use proptest::prelude::*;

/// One randomly generated range bound: unbounded, anchored on an existing
/// key (by index, so it actually lands in the map), or an arbitrary byte
/// string (usually falling between keys or outside the key space).
#[derive(Debug, Clone)]
enum GenBound {
    Unbounded,
    IncludedExisting(prop::sample::Index),
    ExcludedExisting(prop::sample::Index),
    IncludedRandom(Vec<u8>),
    ExcludedRandom(Vec<u8>),
}

fn gen_bound() -> impl Strategy<Value = GenBound> {
    prop_oneof![
        2 => Just(GenBound::Unbounded),
        3 => any::<prop::sample::Index>().prop_map(GenBound::IncludedExisting),
        3 => any::<prop::sample::Index>().prop_map(GenBound::ExcludedExisting),
        2 => proptest::collection::vec(any::<u8>(), 0..40).prop_map(GenBound::IncludedRandom),
        2 => proptest::collection::vec(any::<u8>(), 0..40).prop_map(GenBound::ExcludedRandom),
    ]
}

fn resolve_bound(b: &GenBound, map: &Map) -> Bound<Vec<u8>> {
    let existing = |idx: &prop::sample::Index| -> Vec<u8> {
        if map.is_empty() {
            Vec::new()
        } else {
            let keys: Vec<&Vec<u8>> = map.keys().collect();
            idx.get(&keys).to_vec()
        }
    };
    match b {
        GenBound::Unbounded => Bound::Unbounded,
        GenBound::IncludedExisting(i) => Bound::Included(existing(i)),
        GenBound::ExcludedExisting(i) => Bound::Excluded(existing(i)),
        GenBound::IncludedRandom(k) => Bound::Included(k.clone()),
        GenBound::ExcludedRandom(k) => Bound::Excluded(k.clone()),
    }
}

fn scan_cases(default: u32) -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .map(|v| {
            v.parse()
                .expect("PROPTEST_CASES must be a positive integer")
        })
        .unwrap_or(default);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(scan_cases(64))]

    /// For arbitrary bounds — inside, between and outside the key space,
    /// inclusive/exclusive/unbounded, including inverted ranges — the
    /// forward scan equals the reference filter over the map and the
    /// reverse scan is exactly its reversal.
    #[test]
    fn random_range_scans_match_reference_both_directions(
        n in 0usize..800,
        seed in any::<u64>(),
        start_gen in gen_bound(),
        end_gen in gen_bound(),
    ) {
        let store = MemStore::new();
        let map = bulk_entries(n, seed);
        let root = bulk_load(&store, ChunkParams::default(), map.clone()).expect("bulk_load");

        let start = resolve_bound(&start_gen, &map);
        let end = resolve_bound(&end_gen, &map);

        // Reference semantics by direct filtering (total on inverted
        // ranges, unlike BTreeMap::range, which panics).
        let in_range = |k: &Vec<u8>| {
            (match &start {
                Bound::Included(s) => k >= s,
                Bound::Excluded(s) => k > s,
                Bound::Unbounded => true,
            }) && (match &end {
                Bound::Included(e) => k <= e,
                Bound::Excluded(e) => k < e,
                Bound::Unbounded => true,
            })
        };
        let expected: Vec<(Vec<u8>, Vec<u8>)> = map
            .iter()
            .filter(|(k, _)| in_range(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let range = (
            match &start {
                Bound::Included(k) => Bound::Included(k.as_slice()),
                Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
                Bound::Unbounded => Bound::Unbounded,
            },
            match &end {
                Bound::Included(k) => Bound::Included(k.as_slice()),
                Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
                Bound::Unbounded => Bound::Unbounded,
            },
        );
        let forward: Vec<(Vec<u8>, Vec<u8>)> = scan(&store, &root, range)
            .expect("scan")
            .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
            .collect::<Result<_, _>>()
            .expect("scan items");
        prop_assert_eq!(&forward, &expected, "forward scan diverged from reference");

        let mut reverse: Vec<(Vec<u8>, Vec<u8>)> = scan_rev(&store, &root, range)
            .expect("scan_rev")
            .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
            .collect::<Result<_, _>>()
            .expect("scan_rev items");
        reverse.reverse();
        prop_assert_eq!(&reverse, &expected, "reverse scan is not the reversed forward scan");
    }
}
