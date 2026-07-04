//! History-independence property suite (Load-Bearing Invariant 1).
//!
//! Spec §3.2 (normative): "The split function, serialisation and tree
//! construction MUST be deterministic and history-independent: identical map
//! contents MUST yield identical root hashes regardless of operation order.
//! This property is normative; a property-based test suite enforces it."
//!
//! Ported from the Phase 0 spike suite (spikes/prolly-git-spike) and
//! extended: a deterministic deep-tree (height ≥ 3) case, convergence under
//! non-default chunk parameters (exercising the splice under different
//! boundary behaviour), and cross-store determinism against a second,
//! differently-populated store.
//!
//! Regressions policy: if a property ever fails, proptest writes a
//! `.proptest-regressions` file next to this test — COMMIT that file (the
//! proptest convention) so the counterexample becomes a permanent
//! regression seed. None exists yet because no property has failed.
//!
//! Case counts (64/160/96/32 per property below) are tuned so the whole
//! suite runs in well under a minute in debug mode. Each `proptest!` block
//! takes its count from the `PROPTEST_CASES` environment variable when set
//! (see [`cases`]), so a soak run is e.g.:
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

mod common;

use std::collections::BTreeMap;

use proptest::collection::{btree_map, vec};
use proptest::option;
use proptest::prelude::*;

use acetone_prolly::{BatchOp, ChunkParams, Root, apply_batch, bulk_load, empty, scan};
use common::{Map, MemStore, Rng, bulk_entries, fill_bytes};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Per-test case count: `PROPTEST_CASES` env var if set, else `default`.
/// Defaults are per-property budgets chosen for a fast local run;
/// `PROPTEST_CASES` cranks every property to the same (higher) count for a
/// soak run. A malformed value is a harness bug: fail loudly rather than
/// silently soaking at the default.
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

// ---------------------------------------------------------------------------
// Strategies: target map contents
// ---------------------------------------------------------------------------

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

/// Bulk sizes: mostly small, sometimes hundreds, occasionally thousands.
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
fn apply_history(store: &MemStore, params: ChunkParams, history: Vec<Vec<BatchOp>>) -> Root {
    let mut root = empty(store, params).expect("empty root");
    for batch in history {
        root = apply_batch(store, &root, batch).expect("apply_batch");
    }
    root
}

/// Read the entire map back in key order.
fn scan_all(store: &MemStore, root: &Root) -> Vec<(Vec<u8>, Vec<u8>)> {
    scan(store, root, ..)
        .expect("range_scan")
        .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
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
    /// converge on the identical root hash as one fresh bulk load of the
    /// target contents.
    #[test]
    fn histories_converge_on_bulk_load_root(
        target in target_map(),
        seeds in vec(any::<u64>(), 2..4),
    ) {
        let store = MemStore::new();
        let params = ChunkParams::default();
        let reference = bulk_load(&store, params, target.clone()).expect("bulk_load");

        // Semantic sanity: the reference root really holds the target
        // contents (guards against all paths agreeing on a wrong answer).
        let scanned = scan_all(&store, &reference);
        let expected: Vec<_> = target.clone().into_iter().collect();
        prop_assert_eq!(&scanned, &expected, "bulk_load contents differ from target");

        for seed in seeds {
            let history = random_history(&target, seed);
            let root = apply_history(&store, params, history);
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
    /// original root hash (no residue from the intermediate state).
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
        let store = MemStore::new();
        let params = ChunkParams::default();
        let original = bulk_load(&store, params, base.clone()).expect("bulk_load");

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

        let mutated = apply_batch(&store, &original, ops).expect("apply_batch");
        let reverted = apply_batch(&store, &mutated, inverse).expect("apply_batch inverse");
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
        let store = MemStore::new();
        let params = ChunkParams::default();
        let fresh_empty = empty(&store, params).expect("empty bulk_load");

        let full = bulk_load(&store, params, target.clone()).expect("bulk_load");

        // One-shot delete-everything.
        let one_shot = apply_batch(&store, &full, target.keys().cloned().map(BatchOp::Delete))
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
            root = apply_batch(&store, &root, head.iter().cloned().map(BatchOp::Delete))
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
/// every hash — spec §3.2/§10), not about randomised behaviour. Note the
/// comparison is on the root *hash*: `Root` also carries the params, so
/// whole-struct inequality would be trivially true.
#[test]
fn chunk_params_are_format_defining() {
    let store = MemStore::new();
    let content = bulk_entries(3000, 0xace7_0ae5);
    let default = ChunkParams::default();

    let r_default = bulk_load(&store, default, content.clone()).expect("bulk_load default");
    assert!(
        r_default.height() >= 2,
        "content must span multiple chunks for this test to mean anything"
    );

    // Same params, same content: identical root.
    let r_again = bulk_load(&store, default, content.clone()).expect("bulk_load default again");
    assert_eq!(r_default.hash(), r_again.hash());

    // Different params: different root hash for the same content.
    let variants = [
        ChunkParams::new(
            default.min_bytes(),
            default.mask_bits() + 1,
            default.max_bytes(),
        ),
        ChunkParams::new(
            default.min_bytes(),
            default.mask_bits() - 1,
            default.max_bytes(),
        ),
        ChunkParams::new(
            default.min_bytes() / 2,
            default.mask_bits(),
            default.max_bytes(),
        ),
        ChunkParams::new(
            default.min_bytes(),
            default.mask_bits(),
            default.max_bytes() / 4,
        ),
    ];
    for params in variants {
        let params = params.expect("variant params are valid");
        let r = bulk_load(&store, params, content.clone()).expect("bulk_load variant");
        assert_ne!(
            r_default.hash(),
            r.hash(),
            "params {params:?} produced the same root as the defaults — \
             chunk parameters would silently not be format-defining"
        );
        // And each variant is itself deterministic.
        let r2 = bulk_load(&store, params, content.clone()).expect("bulk_load variant again");
        assert_eq!(r.hash(), r2.hash(), "params {params:?} not deterministic");
    }
}

// ---------------------------------------------------------------------------
// Property 5 — determinism across independent store instances
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(32))]

    /// Content-addressing sanity: the same target content, built via a bulk
    /// load in one fresh store and via a randomised history in another,
    /// yields the same root hash. Nothing about a particular store
    /// (object counts, prior contents) may leak into the hash.
    #[test]
    fn same_content_same_root_across_stores(
        target in small_target_map(),
        seed in any::<u64>(),
    ) {
        let store_a = MemStore::new();
        let store_b = MemStore::new();
        let params = ChunkParams::default();

        // Store A also holds unrelated residue first, so the two stores are
        // genuinely in different states when the target is built.
        bulk_load(&store_a, params, bulk_entries(50, seed ^ 0x5eed))
            .expect("residue bulk_load");

        let root_a = bulk_load(&store_a, params, target.clone()).expect("bulk_load A");
        let root_b = apply_history(&store_b, params, random_history(&target, seed));

        prop_assert_eq!(root_a.hash(), root_b.hash(), "root hashes diverged across stores");
        prop_assert_eq!(root_a.height(), root_b.height(), "heights diverged across stores");
    }
}

// ---------------------------------------------------------------------------
// Property 6 (new) — convergence under non-default chunk parameters
// ---------------------------------------------------------------------------

/// A small set of valid, deliberately awkward parameter profiles: tiny
/// chunks (high boundary churn, exercising the splice constantly), large
/// min relative to max (min-dominated cuts), and a mask so coarse that
/// max_bytes does all the cutting.
fn param_profiles() -> Vec<ChunkParams> {
    vec![
        ChunkParams::new(64, 6, 512).expect("tiny chunks"),
        ChunkParams::new(900, 4, 1024).expect("min-dominated"),
        ChunkParams::new(256, 20, 2048).expect("max-dominated"),
    ]
}

proptest! {
    #![proptest_config(cases(24))]

    /// History independence is a property of the algorithm, not of the
    /// default parameters: randomised histories converge on the bulk-load
    /// root under awkward (but valid) parameter profiles too. Small chunks
    /// force splice boundaries to shift constantly, which is exactly where
    /// a chunker-state or reuse bug would hide.
    #[test]
    fn histories_converge_under_non_default_params(
        target in small_target_map(),
        seed in any::<u64>(),
        profile_idx in 0usize..3,
    ) {
        let store = MemStore::new();
        let params = param_profiles()[profile_idx];
        let reference = bulk_load(&store, params, target.clone()).expect("bulk_load");
        prop_assert_eq!(
            scan_all(&store, &reference),
            target.clone().into_iter().collect::<Vec<_>>()
        );

        let root = apply_history(&store, params, random_history(&target, seed));
        prop_assert_eq!(&reference, &root, "non-default params diverged (seed {})", seed);
    }
}

// ---------------------------------------------------------------------------
// Property 7 (new) — deterministic deep tree (height >= 3)
// ---------------------------------------------------------------------------

/// A deterministic case guaranteed to build a tall tree, so every splice
/// path (leaf level, intermediate levels, root collapse) is exercised
/// without relying on the randomised sizes above: batches applied in two
/// different orders, plus single-key churn at the far edges, all converge.
#[test]
fn deep_tree_histories_converge() {
    let store = MemStore::new();
    let params = ChunkParams::default();
    let target = bulk_entries(20_000, 0xdeef);

    let reference = bulk_load(&store, params, target.clone()).expect("bulk_load");
    assert!(
        reference.height() >= 3,
        "deep-tree case must be height >= 3, got {}",
        reference.height()
    );

    // Route 1: two halves, interleaved keys, applied as separate batches.
    let (evens, odds): (Vec<_>, Vec<_>) = target
        .iter()
        .enumerate()
        .map(|(i, (k, v))| (i, (k.clone(), v.clone())))
        .partition(|(i, _)| i % 2 == 0);
    let evens: Vec<BatchOp> = evens
        .into_iter()
        .map(|(_, (k, v))| BatchOp::Put(k, v))
        .collect();
    let odds: Vec<BatchOp> = odds
        .into_iter()
        .map(|(_, (k, v))| BatchOp::Put(k, v))
        .collect();

    let mut r1 = empty(&store, params).expect("empty");
    r1 = apply_batch(&store, &r1, evens.clone()).expect("evens");
    r1 = apply_batch(&store, &r1, odds.clone()).expect("odds");
    assert_eq!(r1, reference, "evens-then-odds diverged");

    let mut r2 = empty(&store, params).expect("empty");
    r2 = apply_batch(&store, &r2, odds).expect("odds first");
    r2 = apply_batch(&store, &r2, evens).expect("evens second");
    assert_eq!(r2, reference, "odds-then-evens diverged");

    // Route 2: churn at the extreme edges and interior, then revert.
    let first = target.keys().next().expect("first").clone();
    let last = target.keys().next_back().expect("last").clone();
    let mid = target.keys().nth(10_000).expect("mid").clone();
    let mut r3 = reference.clone();
    r3 = apply_batch(
        &store,
        &r3,
        vec![
            BatchOp::Delete(first.clone()),
            BatchOp::Put(b"\x00before-everything".to_vec(), b"x".to_vec()),
            BatchOp::Put(b"zzzz/after-everything".to_vec(), b"y".to_vec()),
            BatchOp::Put(mid.clone(), b"different".to_vec()),
            BatchOp::Delete(last.clone()),
        ],
    )
    .expect("churn");
    assert_ne!(r3, reference);
    r3 = apply_batch(
        &store,
        &r3,
        vec![
            BatchOp::Put(first.clone(), target[&first].clone()),
            BatchOp::Delete(b"\x00before-everything".to_vec()),
            BatchOp::Delete(b"zzzz/after-everything".to_vec()),
            BatchOp::Put(mid.clone(), target[&mid].clone()),
            BatchOp::Put(last.clone(), target[&last].clone()),
        ],
    )
    .expect("revert");
    assert_eq!(r3, reference, "edge churn + revert diverged");
}

// ---------------------------------------------------------------------------
// Regression — long constant keys must not stall level convergence
// ---------------------------------------------------------------------------

/// The spike's per-byte boundary test admitted a deterministic fixed point:
/// with keys longer than the boundary window and eligible cut positions
/// falling inside the (level-invariant) key bytes of inner entries, every
/// level split identically to the one below and the build never converged
/// on a root (found by `histories_converge_under_non_default_params`; the
/// shrunk seed is committed in `.proptest-regressions`). This pins the fix
/// deterministically: long shared-prefix/suffix keys, small chunks.
#[test]
fn long_keys_converge_under_tiny_chunks() {
    let store = MemStore::new();
    let params = ChunkParams::new(64, 6, 512).expect("tiny params");
    for n in [2usize, 3, 7, 50, 300] {
        let map: Map = (0..n)
            .map(|i| {
                // 200-byte keys, mostly constant, differing mid-way.
                let key = format!("{}{:06}{}", "prefix/".repeat(12), i, "/suffix".repeat(15))
                    .into_bytes();
                (key, fill_bytes(i as u64, 40 + i % 60))
            })
            .collect();
        let root = bulk_load(&store, params, map.clone()).expect("bulk_load converges");
        assert!(
            root.height() <= 16,
            "{n} entries built an implausible height {}",
            root.height()
        );
        assert_eq!(
            scan_all(&store, &root),
            map.clone().into_iter().collect::<Vec<_>>()
        );
        // And histories over the same content converge too.
        let via_history = apply_history(&store, params, random_history(&map, 0xf1f0));
        assert_eq!(via_history, root);
    }
}
