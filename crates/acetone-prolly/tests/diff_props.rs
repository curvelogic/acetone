//! Structural-diff property suite (spec §3.2).
//!
//! The load-bearing facts about `diff(a, b)`:
//!
//! - applying it to `a` as a batch reproduces `b` **bit-identically**
//!   (with history independence this makes diff/apply a faithful
//!   round-trip);
//! - it is the symmetric inverse of `diff(b, a)`;
//! - equal roots diff to nothing;
//! - it streams in strictly ascending key order;
//! - its cost is O(changed keys), not O(map size) — asserted with real
//!   read counters;
//! - height-mismatched trees (including the empty tree) diff correctly.

mod common;

use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;

use acetone_prolly::{BatchOp, ChunkParams, DiffEntry, Root, apply_batch, bulk_load, diff, empty};
use common::{Map, MemStore, bulk_entries, fill_bytes};

fn cases(default: u32) -> ProptestConfig {
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

fn build(store: &MemStore, map: &Map) -> Root {
    bulk_load(store, ChunkParams::default(), map.clone()).expect("bulk_load")
}

fn diff_all(store: &MemStore, a: &Root, b: &Root) -> Vec<DiffEntry> {
    diff(store, a, b)
        .expect("diff")
        .collect::<Result<_, _>>()
        .expect("diff entries")
}

/// `(key, before, after)` with owned buffers, for reference comparison.
type Triple = (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);

/// The expected diff computed on reference maps.
fn reference_diff(a: &Map, b: &Map) -> Vec<Triple> {
    let mut out = Vec::new();
    for (k, va) in a {
        match b.get(k) {
            Some(vb) if vb == va => {}
            Some(vb) => out.push((k.clone(), Some(va.clone()), Some(vb.clone()))),
            None => out.push((k.clone(), Some(va.clone()), None)),
        }
    }
    for (k, vb) in b {
        if !a.contains_key(k) {
            out.push((k.clone(), None, Some(vb.clone())));
        }
    }
    out.sort_by(|x, y| x.0.cmp(&y.0));
    out
}

fn as_triples(entries: &[DiffEntry]) -> Vec<Triple> {
    entries
        .iter()
        .map(|e| {
            (
                e.key.to_vec(),
                e.before.as_ref().map(|v| v.to_vec()),
                e.after.as_ref().map(|v| v.to_vec()),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Strategies: a base map and a derived map sharing structure
// ---------------------------------------------------------------------------

fn key() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        1 => Just(Vec::new()),
        8 => vec(any::<u8>(), 1..24),
        3 => vec(any::<u8>(), 24..80),
    ]
}

fn value() -> impl Strategy<Value = Vec<u8>> {
    (any::<u64>(), 0usize..600).prop_map(|(seed, len)| fill_bytes(seed, len))
}

/// A base map plus an edit script deriving a related map: hit existing
/// keys by index (update or delete) and add fresh keys.
#[allow(clippy::type_complexity)]
fn base_and_edits() -> impl Strategy<
    Value = (
        Map,
        Vec<(prop::sample::Index, Option<Vec<u8>>)>,
        Vec<(Vec<u8>, Vec<u8>)>,
    ),
> {
    (
        (0usize..600, any::<u64>()).prop_map(|(n, seed)| bulk_entries(n, seed)),
        vec((any::<prop::sample::Index>(), option::of(value())), 0..24),
        vec((key(), value()), 0..16),
    )
}

fn apply_edits(
    base: &Map,
    hits: &[(prop::sample::Index, Option<Vec<u8>>)],
    adds: &[(Vec<u8>, Vec<u8>)],
) -> Map {
    let mut out = base.clone();
    if !base.is_empty() {
        let keys: Vec<&Vec<u8>> = base.keys().collect();
        for (idx, v) in hits {
            let k = idx.get(&keys).to_vec();
            match v {
                Some(v) => {
                    out.insert(k, v.clone());
                }
                None => {
                    out.remove(&k);
                }
            }
        }
    }
    for (k, v) in adds {
        out.insert(k.clone(), v.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Property 1 — diff matches the reference diff and is ordered
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(64))]

    #[test]
    fn diff_matches_reference_and_is_ordered(
        (base, hits, adds) in base_and_edits(),
    ) {
        let store = MemStore::new();
        let derived = apply_edits(&base, &hits, &adds);
        let ra = build(&store, &base);
        let rb = build(&store, &derived);

        let got = diff_all(&store, &ra, &rb);
        prop_assert_eq!(as_triples(&got), reference_diff(&base, &derived));

        // Strictly ascending keys (also implied by the reference match,
        // but assert it directly so ordering is a first-class guarantee).
        for w in got.windows(2) {
            prop_assert!(w[0].key < w[1].key, "diff keys out of order");
        }
    }
}

// ---------------------------------------------------------------------------
// Property 2 — diff applied to a reproduces b, bit-identically
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(64))]

    #[test]
    fn diff_applied_reproduces_target(
        (base, hits, adds) in base_and_edits(),
    ) {
        let store = MemStore::new();
        let derived = apply_edits(&base, &hits, &adds);
        let ra = build(&store, &base);
        let rb = build(&store, &derived);

        let ops: Vec<BatchOp> = diff_all(&store, &ra, &rb)
            .into_iter()
            .map(|e| match e.after {
                Some(v) => BatchOp::Put(e.key.to_vec(), v.to_vec()),
                None => BatchOp::Delete(e.key.to_vec()),
            })
            .collect();
        let reproduced = apply_batch(&store, &ra, ops).expect("apply diff");
        prop_assert_eq!(reproduced, rb, "diff applied to a must equal b exactly");
    }
}

// ---------------------------------------------------------------------------
// Property 3 — diff is its own symmetric inverse
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(64))]

    #[test]
    fn diff_is_symmetric_inverse(
        (base, hits, adds) in base_and_edits(),
    ) {
        let store = MemStore::new();
        let derived = apply_edits(&base, &hits, &adds);
        let ra = build(&store, &base);
        let rb = build(&store, &derived);

        let forward = diff_all(&store, &ra, &rb);
        let backward = diff_all(&store, &rb, &ra);
        let swapped: Vec<DiffEntry> = backward
            .into_iter()
            .map(|e| DiffEntry { key: e.key, before: e.after, after: e.before })
            .collect();
        prop_assert_eq!(forward, swapped);
    }
}

// ---------------------------------------------------------------------------
// Deterministic cases
// ---------------------------------------------------------------------------

#[test]
fn equal_roots_diff_empty_without_reads() {
    let store = MemStore::new();
    let map = bulk_entries(2000, 5);
    let ra = build(&store, &map);
    let rb = build(&store, &map);
    assert_eq!(ra, rb);

    store.reset_counters();
    assert_eq!(diff_all(&store, &ra, &rb).len(), 0);
    assert_eq!(
        store.reads(),
        0,
        "equal roots must short-circuit on the hash"
    );
}

#[test]
fn diff_handles_height_mismatch() {
    let store = MemStore::new();
    let big = bulk_entries(20_000, 21);
    let small: Map = big
        .iter()
        .take(3)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let r_big = build(&store, &big);
    let r_small = build(&store, &small);
    let r_empty = empty(&store, ChunkParams::default()).expect("empty");
    assert!(r_big.height() >= 3);
    assert_eq!(r_small.height(), 1);

    // tall vs short, both directions.
    let d = diff_all(&store, &r_small, &r_big);
    assert_eq!(as_triples(&d), reference_diff(&small, &big));
    let d = diff_all(&store, &r_big, &r_small);
    assert_eq!(as_triples(&d), reference_diff(&big, &small));

    // anything vs empty, both directions.
    let d = diff_all(&store, &r_empty, &r_big);
    assert_eq!(as_triples(&d), reference_diff(&Map::new(), &big));
    let d = diff_all(&store, &r_big, &r_empty);
    assert_eq!(as_triples(&d), reference_diff(&big, &Map::new()));
    assert_eq!(diff_all(&store, &r_empty, &r_empty).len(), 0);
}

#[test]
fn diff_cost_is_proportional_to_change_not_size() {
    let store = MemStore::new();
    let map = bulk_entries(20_000, 33);
    let ra = build(&store, &map);
    let key = map.keys().nth(10_000).expect("key").clone();
    let rb = apply_batch(&store, &ra, vec![BatchOp::Put(key, b"changed".to_vec())])
        .expect("apply_batch");

    store.reset_counters();
    let d = diff_all(&store, &ra, &rb);
    assert_eq!(d.len(), 1);
    let reads = store.reads();
    let height = u64::from(ra.height());
    // Both sides descend only their changed spines (plus boundary
    // alignment): far below the ~350 internal/leaf chunks of this map.
    assert!(
        reads <= 6 * height + 8,
        "single-key diff read {reads} chunks over {} total",
        store.len()
    );
}

#[test]
fn diff_streams_lazily() {
    // The iterator must not pre-compute everything: after taking the first
    // entry of a whole-map diff, reads stay bounded by the entry's spine
    // region rather than the full tree.
    let store = MemStore::new();
    let map = bulk_entries(20_000, 55);
    let ra = empty(&store, ChunkParams::default()).expect("empty");
    let rb = build(&store, &map);
    let total = store.len() as u64;

    store.reset_counters();
    let mut it = diff(&store, &ra, &rb).expect("diff");
    let first = it.next().expect("one entry").expect("ok");
    assert_eq!(
        first.key.as_ref(),
        map.keys().next().expect("first").as_slice()
    );
    assert!(
        store.reads() < total / 2,
        "first diff entry forced {} of {} chunk reads",
        store.reads(),
        total
    );
}
