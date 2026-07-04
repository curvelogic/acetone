//! Three-way-merge property suite (Load-Bearing Invariant 4).
//!
//! `merge(base, ours, theirs)` MUST be a pure function of its three roots:
//! same inputs, same merged root, same ordered conflict stream — however
//! the inputs were built and whatever store they sit in. Conflicts are
//! data, not errors; conflicted keys are excluded from the merged root
//! (the documented materialisation contract).

mod common;

use std::collections::BTreeMap;

use proptest::collection::vec;
use proptest::option;
use proptest::prelude::*;

use acetone_prolly::{
    BatchOp, ChunkParams, Conflict, MergeOutcome, ProllyError, Root, apply_batch, bulk_load, get,
    merge, scan,
};
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

fn scan_all(store: &MemStore, root: &Root) -> Vec<(Vec<u8>, Vec<u8>)> {
    scan(store, root, ..)
        .expect("scan")
        .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
        .collect::<Result<_, _>>()
        .expect("scan items")
}

/// One side's edit: `Some(v)` = put, `None` = delete.
type Edit = Option<Vec<u8>>;
type EditScript = BTreeMap<Vec<u8>, Edit>;

/// Apply an edit script to a reference map.
fn apply_script(base: &Map, script: &EditScript) -> Map {
    let mut out = base.clone();
    for (k, e) in script {
        match e {
            Some(v) => {
                out.insert(k.clone(), v.clone());
            }
            None => {
                out.remove(k);
            }
        }
    }
    out
}

/// The reference three-way merge over maps, mirroring the documented
/// semantics: per key, one-sided changes win; identical changes are clean;
/// divergent changes are conflicts and the key is excluded from the merged
/// map.
fn reference_merge(base: &Map, ours: &Map, theirs: &Map) -> (Map, Vec<Vec<u8>>) {
    let mut merged = base.clone();
    let mut conflicts = Vec::new();
    let mut keys: Vec<&Vec<u8>> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        let b = base.get(k);
        let o = ours.get(k);
        let t = theirs.get(k);
        let ours_changed = o != b;
        let theirs_changed = t != b;
        match (ours_changed, theirs_changed) {
            (false, false) => {}
            (true, false) => {
                match o {
                    Some(v) => merged.insert(k.clone(), v.clone()),
                    None => merged.remove(k),
                };
            }
            (false, true) => {
                match t {
                    Some(v) => merged.insert(k.clone(), v.clone()),
                    None => merged.remove(k),
                };
            }
            (true, true) => {
                if o == t {
                    match o {
                        Some(v) => merged.insert(k.clone(), v.clone()),
                        None => merged.remove(k),
                    };
                } else {
                    merged.remove(k);
                    conflicts.push(k.clone());
                }
            }
        }
    }
    (merged, conflicts)
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn value() -> impl Strategy<Value = Vec<u8>> {
    (any::<u64>(), 0usize..400).prop_map(|(seed, len)| fill_bytes(seed, len))
}

fn fresh_key() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        8 => vec(any::<u8>(), 1..24),
        2 => vec(any::<u8>(), 24..80),
    ]
}

/// A base map plus two edit scripts. Scripts mix index-addressed hits on
/// base keys (guaranteeing overlap, hence conflicts) with fresh keys.
#[allow(clippy::type_complexity)]
fn merge_case() -> impl Strategy<
    Value = (
        Map,
        Vec<(prop::sample::Index, Edit)>,
        Vec<(Vec<u8>, Edit)>,
        Vec<(prop::sample::Index, Edit)>,
        Vec<(Vec<u8>, Edit)>,
    ),
> {
    (
        (0usize..400, any::<u64>()).prop_map(|(n, seed)| bulk_entries(n, seed)),
        vec((any::<prop::sample::Index>(), option::of(value())), 0..16),
        vec((fresh_key(), option::of(value())), 0..10),
        vec((any::<prop::sample::Index>(), option::of(value())), 0..16),
        vec((fresh_key(), option::of(value())), 0..10),
    )
}

fn to_script(
    base: &Map,
    hits: &[(prop::sample::Index, Edit)],
    fresh: &[(Vec<u8>, Edit)],
) -> EditScript {
    let mut script = EditScript::new();
    if !base.is_empty() {
        let keys: Vec<&Vec<u8>> = base.keys().collect();
        for (idx, e) in hits {
            script.insert(idx.get(&keys).to_vec(), e.clone());
        }
    }
    for (k, e) in fresh {
        script.insert(k.clone(), e.clone());
    }
    script
}

fn script_ops(script: &EditScript) -> Vec<BatchOp> {
    script
        .iter()
        .map(|(k, e)| match e {
            Some(v) => BatchOp::Put(k.clone(), v.clone()),
            None => BatchOp::Delete(k.clone()),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Property 1 — merge matches the reference semantics
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(48))]

    #[test]
    fn merge_matches_reference_semantics(
        (base, o_hits, o_fresh, t_hits, t_fresh) in merge_case(),
    ) {
        let store = MemStore::new();
        let o_script = to_script(&base, &o_hits, &o_fresh);
        let t_script = to_script(&base, &t_hits, &t_fresh);
        let ours_map = apply_script(&base, &o_script);
        let theirs_map = apply_script(&base, &t_script);

        let r_base = build(&store, &base);
        let r_ours = build(&store, &ours_map);
        let r_theirs = build(&store, &theirs_map);

        let outcome = merge(&store, &r_base, &r_ours, &r_theirs).expect("merge");
        let (expected_map, expected_conflicts) = reference_merge(&base, &ours_map, &theirs_map);

        // The merged root holds exactly the reference contents, and is
        // bit-identical to a fresh build of them (history independence).
        prop_assert_eq!(
            scan_all(&store, &outcome.root),
            expected_map.clone().into_iter().collect::<Vec<_>>()
        );
        prop_assert_eq!(&outcome.root, &build(&store, &expected_map));

        // Conflict records: right keys, right order, right three values.
        let got_keys: Vec<Vec<u8>> =
            outcome.conflicts.iter().map(|c| c.key.to_vec()).collect();
        prop_assert_eq!(&got_keys, &expected_conflicts);
        for w in outcome.conflicts.windows(2) {
            prop_assert!(w[0].key < w[1].key, "conflict stream out of order");
        }
        for c in &outcome.conflicts {
            let k = c.key.to_vec();
            prop_assert_eq!(c.base.as_ref().map(|v| v.to_vec()), base.get(&k).cloned());
            prop_assert_eq!(c.ours.as_ref().map(|v| v.to_vec()), ours_map.get(&k).cloned());
            prop_assert_eq!(c.theirs.as_ref().map(|v| v.to_vec()), theirs_map.get(&k).cloned());
            // The contract: conflicted keys are absent from the merged root.
            prop_assert_eq!(get(&store, &outcome.root, &k).expect("get"), None);
        }
    }
}

// ---------------------------------------------------------------------------
// Property 2 — determinism: same inputs, same outcome, any store state
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(32))]

    #[test]
    fn merge_is_deterministic_across_builds_and_stores(
        (base, o_hits, o_fresh, t_hits, t_fresh) in merge_case(),
        residue_seed in any::<u64>(),
    ) {
        let o_script = to_script(&base, &o_hits, &o_fresh);
        let t_script = to_script(&base, &t_hits, &t_fresh);

        // Store 1: sides built by bulk load.
        let s1 = MemStore::new();
        let b1 = build(&s1, &base);
        let o1 = build(&s1, &apply_script(&base, &o_script));
        let t1 = build(&s1, &apply_script(&base, &t_script));
        let m1 = merge(&s1, &b1, &o1, &t1).expect("merge 1");

        // Store 2: different residue, sides built by batch application on
        // top of base (a different history to the same roots).
        let s2 = MemStore::new();
        build(&s2, &bulk_entries(40, residue_seed));
        let b2 = build(&s2, &base);
        let o2 = apply_batch(&s2, &b2, script_ops(&o_script)).expect("ours via batch");
        let t2 = apply_batch(&s2, &b2, script_ops(&t_script)).expect("theirs via batch");
        prop_assert_eq!(&o1.hash(), &o2.hash(), "side roots must agree first");
        prop_assert_eq!(&t1.hash(), &t2.hash());
        let m2 = merge(&s2, &b2, &o2, &t2).expect("merge 2");

        prop_assert_eq!(&m1.root, &m2.root, "merged roots diverged");
        prop_assert_eq!(&m1.conflicts, &m2.conflicts, "conflict streams diverged");
    }
}

// ---------------------------------------------------------------------------
// Property 3 — clean merge of disjoint batches == direct application
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(48))]

    /// When the two sides touch disjoint key sets, the merge is clean and
    /// the merged root is bit-identical to applying both batches directly
    /// to base.
    #[test]
    fn clean_merge_of_disjoint_batches_equals_direct_application(
        base in (0usize..400, any::<u64>()).prop_map(|(n, seed)| bulk_entries(n, seed)),
        ours_edits in vec((fresh_key(), option::of(value())), 0..16),
        theirs_edits in vec((fresh_key(), option::of(value())), 0..16),
    ) {
        let store = MemStore::new();
        // Make the key sets disjoint by prefixing each side.
        let o_script: EditScript = ours_edits
            .into_iter()
            .map(|(k, e)| ([b"ours/".to_vec(), k].concat(), e))
            .collect();
        let t_script: EditScript = theirs_edits
            .into_iter()
            .map(|(k, e)| ([b"theirs/".to_vec(), k].concat(), e))
            .collect();

        let r_base = build(&store, &base);
        let r_ours = apply_batch(&store, &r_base, script_ops(&o_script)).expect("ours");
        let r_theirs = apply_batch(&store, &r_base, script_ops(&t_script)).expect("theirs");

        let outcome = merge(&store, &r_base, &r_ours, &r_theirs).expect("merge");
        prop_assert!(outcome.conflicts.is_empty(), "disjoint edits must merge cleanly");

        let mut both = script_ops(&o_script);
        both.extend(script_ops(&t_script));
        let direct = apply_batch(&store, &r_base, both).expect("direct application");
        prop_assert_eq!(outcome.root, direct);
    }
}

// ---------------------------------------------------------------------------
// Deterministic semantics cases
// ---------------------------------------------------------------------------

fn simple_case() -> (MemStore, Root, Map) {
    let store = MemStore::new();
    let mut base = bulk_entries(300, 77);
    base.insert(b"key/shared".to_vec(), b"original".to_vec());
    let root = build(&store, &base);
    (store, root, base)
}

fn one_op(store: &MemStore, root: &Root, op: BatchOp) -> Root {
    apply_batch(store, root, vec![op]).expect("apply")
}

fn merged(store: &MemStore, base: &Root, ours: &Root, theirs: &Root) -> MergeOutcome {
    merge(store, base, ours, theirs).expect("merge")
}

#[test]
fn one_sided_change_is_taken() {
    let (store, base, _) = simple_case();
    let ours = one_op(
        &store,
        &base,
        BatchOp::Put(b"key/shared".to_vec(), b"ours".to_vec()),
    );
    let out = merged(&store, &base, &ours, &base);
    assert!(out.conflicts.is_empty());
    assert_eq!(out.root, ours, "ours-only change: merged == ours");

    let out = merged(&store, &base, &base, &ours);
    assert!(out.conflicts.is_empty());
    assert_eq!(out.root, ours, "theirs-only change: merged == theirs");
}

#[test]
fn identical_changes_are_clean() {
    let (store, base, _) = simple_case();
    let a = one_op(
        &store,
        &base,
        BatchOp::Put(b"key/shared".to_vec(), b"same".to_vec()),
    );
    let b = one_op(
        &store,
        &base,
        BatchOp::Put(b"key/shared".to_vec(), b"same".to_vec()),
    );
    assert_eq!(a, b);
    let out = merged(&store, &base, &a, &b);
    assert!(out.conflicts.is_empty());
    assert_eq!(out.root, a);

    // Both sides deleting is also an identical change.
    let da = one_op(&store, &base, BatchOp::Delete(b"key/shared".to_vec()));
    let db = one_op(&store, &base, BatchOp::Delete(b"key/shared".to_vec()));
    let out = merged(&store, &base, &da, &db);
    assert!(out.conflicts.is_empty());
    assert_eq!(out.root, da);
}

#[test]
fn divergent_puts_conflict_and_key_is_excluded() {
    let (store, base, base_map) = simple_case();
    let ours = one_op(
        &store,
        &base,
        BatchOp::Put(b"key/shared".to_vec(), b"ours".to_vec()),
    );
    let theirs = one_op(
        &store,
        &base,
        BatchOp::Put(b"key/shared".to_vec(), b"theirs".to_vec()),
    );
    let out = merged(&store, &base, &ours, &theirs);
    assert_eq!(
        out.conflicts,
        vec![Conflict {
            key: b"key/shared".to_vec().into(),
            base: Some(b"original".to_vec().into()),
            ours: Some(b"ours".to_vec().into()),
            theirs: Some(b"theirs".to_vec().into()),
        }]
    );
    assert_eq!(get(&store, &out.root, b"key/shared").expect("get"), None);
    // Everything else is untouched: merged == base minus the key.
    let mut expected = base_map;
    expected.remove(b"key/shared".as_slice());
    assert_eq!(out.root, build(&store, &expected));
}

#[test]
fn delete_vs_modify_conflicts() {
    let (store, base, _) = simple_case();
    let ours = one_op(&store, &base, BatchOp::Delete(b"key/shared".to_vec()));
    let theirs = one_op(
        &store,
        &base,
        BatchOp::Put(b"key/shared".to_vec(), b"kept".to_vec()),
    );
    let out = merged(&store, &base, &ours, &theirs);
    assert_eq!(
        out.conflicts,
        vec![Conflict {
            key: b"key/shared".to_vec().into(),
            base: Some(b"original".to_vec().into()),
            ours: None,
            theirs: Some(b"kept".to_vec().into()),
        }]
    );
    assert_eq!(get(&store, &out.root, b"key/shared").expect("get"), None);
}

#[test]
fn divergent_adds_conflict_with_no_base_value() {
    let (store, base, _) = simple_case();
    let ours = one_op(
        &store,
        &base,
        BatchOp::Put(b"new/key".to_vec(), b"a".to_vec()),
    );
    let theirs = one_op(
        &store,
        &base,
        BatchOp::Put(b"new/key".to_vec(), b"b".to_vec()),
    );
    let out = merged(&store, &base, &ours, &theirs);
    assert_eq!(out.conflicts.len(), 1);
    assert_eq!(out.conflicts[0].base, None);
    assert_eq!(get(&store, &out.root, b"new/key").expect("get"), None);
    // Nothing else changed, so the merged root is base itself.
    assert_eq!(out.root, base);
}

#[test]
fn merge_of_identical_sides_is_that_side() {
    let (store, base, _) = simple_case();
    let side = one_op(&store, &base, BatchOp::Put(b"k".to_vec(), b"v".to_vec()));
    let out = merged(&store, &base, &side, &side);
    assert!(out.conflicts.is_empty());
    assert_eq!(out.root, side);

    // And all-equal inputs merge to themselves.
    let out = merged(&store, &base, &base, &base);
    assert!(out.conflicts.is_empty());
    assert_eq!(out.root, base);
}

#[test]
fn params_mismatch_is_a_typed_error() {
    let store = MemStore::new();
    let map = bulk_entries(50, 1);
    let base = build(&store, &map);
    let other_params = ChunkParams::new(64, 6, 512).expect("params");
    let odd = bulk_load(&store, other_params, map.clone()).expect("bulk_load");
    let err = merge(&store, &base, &odd, &base).expect_err("params mismatch");
    assert!(matches!(err, ProllyError::ParamsMismatch));
}

#[test]
fn merge_cost_is_proportional_to_changes() {
    let store = MemStore::new();
    let map = bulk_entries(20_000, 88);
    let base = build(&store, &map);
    let k1 = map.keys().nth(3_000).expect("k1").clone();
    let k2 = map.keys().nth(17_000).expect("k2").clone();
    let ours = one_op(&store, &base, BatchOp::Put(k1, b"ours-change".to_vec()));
    let theirs = one_op(&store, &base, BatchOp::Put(k2, b"theirs-change".to_vec()));

    store.reset_counters();
    let out = merged(&store, &base, &ours, &theirs);
    assert!(out.conflicts.is_empty());
    let reads = store.reads();
    let height = u64::from(base.height());
    // Two single-key diffs plus one two-key batch: a few spines, not the
    // ~700 chunks of this map.
    assert!(
        reads <= 16 * height + 16,
        "two-key merge read {reads} chunks of {}",
        store.len()
    );
}
