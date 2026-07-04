//! History-independence property suite (bead acetone-28x.5, Phase 0).
//!
//! Spec §3.2 (normative): "The split function, serialisation and tree
//! construction MUST be deterministic and history-independent: identical map
//! contents MUST yield identical root hashes regardless of operation order.
//! This property is normative; a property-based test suite enforces it."
//!
//! This suite is the seed of the normative `acetone-prolly` property suite
//! (Phase 1): the strategies (target maps with chunk-boundary-spanning value
//! sizes, randomised convergent histories, batch inversion) should carry
//! over; only the store construction and API names will change.
//!
//! Case counts (64/160/96/32 per property below) are tuned so the whole
//! suite runs in well under a minute in debug mode (~25 s on an M-series
//! laptop). Each `proptest!` block takes its count from the
//! `PROPTEST_CASES` environment variable when set (see [`cases`]), so a
//! soak run is e.g.:
//!
//! ```text
//! PROPTEST_CASES=1000 cargo test --test history_independence
//! ```
//!
//! Generator design favours shrinkability: proptest generates *small*
//! structures (a target map, a handful of u64 seeds, batch descriptions) and
//! bulky/ordering detail is derived deterministically from seeds via a local
//! splitmix64 stream. A failing case therefore shrinks to a small map and a
//! seed, both of which replay exactly.

use std::collections::BTreeMap;

use proptest::collection::{btree_map, vec};
use proptest::option;
use proptest::prelude::*;

use prolly_git_spike::chunker::ChunkParams;
use prolly_git_spike::{BatchOp, Root, Store};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn new_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::create(&dir.path().join("repo.git")).expect("create store");
    (dir, store)
}

/// Per-test case count: `PROPTEST_CASES` env var if set, else `default`.
/// Defaults here are per-property budgets chosen for a fast local run;
/// `PROPTEST_CASES` cranks every property to the same (higher) count for a
/// soak run.
fn cases(default: u32) -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Deterministic pseudo-randomness (splitmix64) — bulk detail derived from
// seeds so proptest only has to generate (and shrink) small values.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform-ish in `0..n` (`n > 0`).
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// `len` deterministic pseudo-random bytes.
fn fill_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let chunk = rng.next_u64().to_be_bytes();
        let take = chunk.len().min(len - out.len());
        out.extend_from_slice(&chunk[..take]);
    }
    out
}

// ---------------------------------------------------------------------------
// Strategies: target map contents
// ---------------------------------------------------------------------------

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

/// Value lengths spanning the chunk-boundary-relevant sizes for the default
/// parameters (min 1024, ~4 KiB mean, max 16384): empty, small, around
/// `min_bytes`, around the mean, and larger than `max_bytes` (a single entry
/// bigger than a whole chunk, forcing an over-full cut).
fn value_len() -> impl Strategy<Value = usize> {
    prop_oneof![
        3 => Just(0usize),
        8 => 1usize..64,
        2 => 900usize..1200,
        2 => 3500usize..5000,
        1 => 17_000usize..20_000,
    ]
}

/// A value as (seed, len) expanded deterministically — shrinks on both.
fn value() -> impl Strategy<Value = Vec<u8>> {
    (any::<u64>(), value_len()).prop_map(|(seed, len)| fill_bytes(seed, len))
}

/// Arbitrary byte-string keys, mostly short, occasionally long or empty
/// (the empty key is a valid key and sorts first).
fn key() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        1 => Just(Vec::new()),
        8 => vec(any::<u8>(), 1..24),
        3 => vec(any::<u8>(), 24..80),
    ]
}

/// A small map with adversarial keys and boundary-spanning values.
fn small_map() -> impl Strategy<Value = Map> {
    btree_map(key(), value(), 0..48)
}

/// `n` deterministic bulk entries (distinct keys, ~40–100-byte values) that
/// vary with `seed`. Cheap to generate and to shrink (only `n` and `seed`
/// are proptest-generated), used to push map sizes up to ~5k entries.
fn bulk_entries(n: usize, seed: u64) -> Map {
    let mut rng = Rng::new(seed);
    let mut m = Map::new();
    for i in 0..n {
        let key = format!("bulk/{seed:016x}/{i:06}").into_bytes();
        let vlen = 40 + rng.below(60);
        let vseed = rng.next_u64();
        m.insert(key, fill_bytes(vseed, vlen));
    }
    m
}

/// Bulk sizes: mostly small, sometimes hundreds, occasionally thousands
/// (up to the ~5k-entry envelope the bead asks for).
fn bulk_size() -> impl Strategy<Value = usize> {
    prop_oneof![
        6 => 0usize..100,
        3 => 100usize..800,
        1 => 800usize..5000,
    ]
}

/// A target map: deterministic bulk entries (size scaling) merged with an
/// explicitly generated small map (key/value-shape coverage).
fn target_map() -> impl Strategy<Value = Map> {
    (small_map(), bulk_size(), any::<u64>()).prop_map(|(small, n, seed)| {
        let mut m = bulk_entries(n, seed);
        m.extend(small);
        m
    })
}

/// A cheaper target map for the properties that build several trees per
/// case: bulk part capped at a few hundred entries.
fn small_target_map() -> impl Strategy<Value = Map> {
    (small_map(), 0usize..300, any::<u64>()).prop_map(|(small, n, seed)| {
        let mut m = bulk_entries(n, seed);
        m.extend(small);
        m
    })
}

// ---------------------------------------------------------------------------
// Randomised convergent histories
// ---------------------------------------------------------------------------

/// Build a random operation history, derived entirely from `seed`, that is
/// guaranteed to converge on exactly `target` when applied to an empty map:
///
/// - every target key gets a final `Put` of its target value, optionally
///   preceded by wrong-value `Put`s and/or `Delete`s (delete-then-reinsert);
/// - extra keys *not* in the target are inserted (possibly updated) and then
///   deleted;
/// - per-key op order is preserved, but ops of different keys are randomly
///   interleaved (so insertion order across keys is arbitrary);
/// - the interleaved stream is split into batches at random points
///   (different batch partitions of the same stream are themselves distinct
///   histories).
fn random_history(target: &Map, seed: u64) -> Vec<Vec<BatchOp>> {
    let mut rng = Rng::new(seed);

    // Per-key op queues; within a queue order must be preserved.
    let mut queues: Vec<Vec<BatchOp>> = Vec::new();
    for (k, v) in target {
        let mut q = Vec::new();
        if rng.below(3) == 0 {
            for _ in 0..1 + rng.below(2) {
                if rng.below(3) == 0 {
                    q.push(BatchOp::Delete(k.clone()));
                } else {
                    let wrong = fill_bytes(rng.next_u64(), rng.below(200));
                    q.push(BatchOp::Put(k.clone(), wrong));
                }
            }
        }
        q.push(BatchOp::Put(k.clone(), v.clone()));
        queues.push(q);
    }

    // Churn on keys absent from the target: insert/update then delete.
    for _ in 0..rng.below(24) {
        let k = fill_bytes(rng.next_u64(), 1 + rng.below(40));
        if target.contains_key(&k) {
            continue; // vanishingly unlikely; keep convergence trivially true
        }
        let mut q = vec![BatchOp::Put(
            k.clone(),
            fill_bytes(rng.next_u64(), rng.below(300)),
        )];
        if rng.below(2) == 0 {
            q.push(BatchOp::Put(
                k.clone(),
                fill_bytes(rng.next_u64(), rng.below(300)),
            ));
        }
        q.push(BatchOp::Delete(k));
        queues.push(q);
    }

    // Random interleave preserving per-key order.
    let mut cursors: Vec<(usize, usize)> = (0..queues.len()).map(|i| (i, 0)).collect();
    let total: usize = queues.iter().map(Vec::len).sum();
    let mut ops: Vec<BatchOp> = Vec::with_capacity(total);
    while !cursors.is_empty() {
        let pick = rng.below(cursors.len());
        let (qi, ref mut oi) = cursors[pick];
        ops.push(queues[qi][*oi].clone());
        *oi += 1;
        if *oi == queues[qi].len() {
            cursors.swap_remove(pick);
        }
    }

    // Partition into 1..=40 batches at random cut points.
    if ops.is_empty() {
        return Vec::new();
    }
    let n_batches = 1 + rng.below(ops.len().min(40));
    let mut cuts: Vec<usize> = (0..n_batches - 1).map(|_| rng.below(ops.len())).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut batches = Vec::with_capacity(cuts.len() + 1);
    let mut rest = ops;
    for cut in cuts.into_iter().rev() {
        let tail = rest.split_off(cut);
        if !tail.is_empty() {
            batches.push(tail);
        }
    }
    if !rest.is_empty() {
        batches.push(rest);
    }
    batches.reverse();
    batches
}

/// Apply a history starting from the empty map; return the final root.
fn apply_history(store: &Store, history: Vec<Vec<BatchOp>>) -> Root {
    let mut root = store.bulk_load(Vec::new()).expect("empty bulk_load");
    for batch in history {
        root = store.apply_batch(&root, batch).expect("apply_batch");
    }
    root
}

/// Read the entire map back in key order.
fn scan_all(store: &Store, root: &Root) -> Vec<(Vec<u8>, Vec<u8>)> {
    store
        .range_scan(root, ..)
        .expect("range_scan")
        .collect::<Result<_, _>>()
        .expect("scan items")
}

// ---------------------------------------------------------------------------
// Property 1 — core history independence (spec §3.2, normative)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(64))]

    /// Any number of distinct randomly generated histories — different
    /// insertion orders, different batch partitions, wrong-value updates,
    /// delete-then-reinsert, churn on keys outside the final map — MUST
    /// converge on the identical root OID as one fresh bulk load of the
    /// target contents.
    #[test]
    fn histories_converge_on_bulk_load_root(
        target in target_map(),
        seeds in vec(any::<u64>(), 2..4),
    ) {
        let (_dir, store) = new_store();
        let reference = store
            .bulk_load(target.clone())
            .expect("bulk_load");

        // Semantic sanity: the reference root really holds the target
        // contents (guards against all paths agreeing on a wrong answer).
        let scanned = scan_all(&store, &reference);
        let expected: Vec<_> = target.clone().into_iter().collect();
        prop_assert_eq!(&scanned, &expected, "bulk_load contents differ from target");

        for seed in seeds {
            let history = random_history(&target, seed);
            let root = apply_history(&store, history);
            prop_assert_eq!(
                &reference, &root,
                "history (seed {}) diverged from bulk-load root", seed
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 2 — mutate then revert returns the exact original root
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(160))]

    /// Applying a random batch and then its exact inverse restores the
    /// original root OID (no residue from the intermediate state).
    #[test]
    fn mutate_then_revert_restores_root(
        base in small_target_map(),
        // A batch as (key, Some(value) = put | None = delete); keys may hit
        // or miss the base map.
        batch in btree_map(key(), option::of(value()), 1..32),
        // Extra ops targeting keys that definitely exist in the base map,
        // chosen by index — otherwise random keys almost never collide.
        touch_existing in vec((any::<prop::sample::Index>(), option::of(value())), 0..8),
    ) {
        let (_dir, store) = new_store();
        let original = store
            .bulk_load(base.clone())
            .expect("bulk_load");

        let mut effective: BTreeMap<Vec<u8>, Option<Vec<u8>>> = batch;
        if !base.is_empty() {
            let keys: Vec<&Vec<u8>> = base.keys().collect();
            for (idx, val) in touch_existing {
                effective.insert(idx.get(&keys).to_vec(), val);
            }
        }

        let ops: Vec<BatchOp> = effective
            .iter()
            .map(|(k, v)| match v {
                Some(v) => BatchOp::Put(k.clone(), v.clone()),
                None => BatchOp::Delete(k.clone()),
            })
            .collect();
        // Exact inverse: restore the base's value where one existed, delete
        // where the key was absent.
        let inverse: Vec<BatchOp> = effective
            .keys()
            .map(|k| match base.get(k) {
                Some(v) => BatchOp::Put(k.clone(), v.clone()),
                None => BatchOp::Delete(k.clone()),
            })
            .collect();

        let mutated = store.apply_batch(&original, ops).expect("apply_batch");
        let reverted = store
            .apply_batch(&mutated, inverse)
            .expect("apply_batch inverse");
        prop_assert_eq!(&original, &reverted, "revert did not restore the root");
    }
}

// ---------------------------------------------------------------------------
// Property 3 — empty-map root stability
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(96))]

    /// All routes to the empty map agree: a fresh empty bulk load, building
    /// then deleting everything in one batch, and deleting everything in
    /// randomly partitioned batches.
    #[test]
    fn all_routes_to_empty_agree(
        target in small_target_map(),
        seed in any::<u64>(),
    ) {
        let (_dir, store) = new_store();
        let fresh_empty = store.bulk_load(Vec::new()).expect("empty bulk_load");

        let full = store
            .bulk_load(target.clone())
            .expect("bulk_load");

        // One-shot delete-everything.
        let one_shot = store
            .apply_batch(&full, target.keys().cloned().map(BatchOp::Delete))
            .expect("apply_batch");
        prop_assert_eq!(&fresh_empty, &one_shot, "one-shot emptying diverged");

        // Delete everything in randomly sized batches, in shuffled order.
        let mut rng = Rng::new(seed);
        let mut keys: Vec<Vec<u8>> = target.keys().cloned().collect();
        for i in (1..keys.len()).rev() {
            let j = rng.below(i + 1);
            keys.swap(i, j);
        }
        let mut root = full;
        let mut rest = keys.as_slice();
        while !rest.is_empty() {
            let n = 1 + rng.below(rest.len());
            let (head, tail) = rest.split_at(n);
            root = store
                .apply_batch(&root, head.iter().cloned().map(BatchOp::Delete))
                .expect("apply_batch");
            rest = tail;
        }
        prop_assert_eq!(&fresh_empty, &root, "batched emptying diverged");
    }
}

// ---------------------------------------------------------------------------
// Property 4 — chunk parameters are format-defining
// ---------------------------------------------------------------------------

/// Same parameters always agree; different parameters produce different
/// roots for the same (multi-chunk) content. The divergence half is a plain
/// deterministic test: it needs content large enough to be chunked, and it
/// is an assertion about the format (changing chunk parameters changes
/// every hash — spec §3.2/§7), not about randomised behaviour. Note the
/// comparison is on the root *OID*: `Root` also carries the params, so
/// whole-struct inequality would be trivially true.
#[test]
fn chunk_params_are_format_defining() {
    let (_dir, store) = new_store();
    let content = bulk_entries(3000, 0xace7_0ae5);
    let default = ChunkParams::default();

    let r_default = store
        .bulk_load_with(default, content.clone())
        .expect("bulk_load_with default");
    assert!(
        r_default.height >= 2,
        "content must span multiple chunks for this test to mean anything"
    );

    // Same params, same content: identical root (and identical to the
    // params-defaulted entry point).
    let r_again = store
        .bulk_load_with(default, content.clone())
        .expect("bulk_load_with default again");
    assert_eq!(r_default.oid, r_again.oid);
    let r_plain = store.bulk_load(content.clone()).expect("bulk_load");
    assert_eq!(r_default.oid, r_plain.oid);

    // Different params: different root OID for the same content.
    let variants = [
        ChunkParams {
            mask_bits: default.mask_bits + 1,
            ..default
        },
        ChunkParams {
            mask_bits: default.mask_bits - 1,
            ..default
        },
        ChunkParams {
            min_bytes: default.min_bytes / 2,
            ..default
        },
        ChunkParams {
            max_bytes: default.max_bytes / 4,
            ..default
        },
    ];
    for params in variants {
        let r = store
            .bulk_load_with(params, content.clone())
            .expect("bulk_load_with variant");
        assert_ne!(
            r_default.oid, r.oid,
            "params {params:?} produced the same root as the defaults — \
             chunk parameters would silently not be format-defining"
        );
        // And each variant is itself deterministic.
        let r2 = store
            .bulk_load_with(params, content.clone())
            .expect("bulk_load_with variant again");
        assert_eq!(r.oid, r2.oid, "params {params:?} not deterministic");
    }
}

// ---------------------------------------------------------------------------
// Property 5 — determinism across independent store instances
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(32))]

    /// Content-addressing sanity: the same target content, built via a bulk
    /// load in one fresh repository and via a randomised history in another,
    /// yields the same root OID. Nothing about a particular repository
    /// (paths, object counts, prior contents) may leak into the hash.
    #[test]
    fn same_content_same_root_across_stores(
        target in small_target_map(),
        seed in any::<u64>(),
    ) {
        let (_dir_a, store_a) = new_store();
        let (_dir_b, store_b) = new_store();

        // Store A also holds unrelated residue first, so the two stores are
        // genuinely in different states when the target is built.
        store_a
            .bulk_load(bulk_entries(50, seed ^ 0x5eed))
            .expect("residue bulk_load");

        let root_a = store_a
            .bulk_load(target.clone())
            .expect("bulk_load A");
        let root_b = apply_history(&store_b, random_history(&target, seed));

        prop_assert_eq!(root_a.oid, root_b.oid, "root OIDs diverged across stores");
        prop_assert_eq!(root_a.height, root_b.height, "heights diverged across stores");
    }
}
