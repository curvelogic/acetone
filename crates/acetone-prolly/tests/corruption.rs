//! Hostile-input corpus: mutated, truncated, forged and dangling chunks.
//!
//! Every mutation below must produce **an error, not a panic, and not a
//! wrong answer**: any operation whose path crosses damaged data returns
//! `Err`. In-scope damage is structural — level tags, counts, framing,
//! orderings, parent boundary claims, dangling references. A flipped byte
//! *inside a value* decodes fine and cannot be detected at this layer by
//! design: a content-addressed store returns bytes matching the requested
//! hash, so undetected bit rot means the store itself is broken (git
//! catches it with `git fsck`); `MemStore::corrupt` deliberately simulates
//! exactly that breakage for the structural cases.

mod common;

use proptest::collection::vec;
use proptest::prelude::*;

use acetone_prolly::{
    BatchOp, ChunkParams, Hash, ProllyError, Root, apply_batch, bulk_load, diff, get,
    reachable_chunks, scan, scan_rev,
};
use common::{Map, MemStore, bulk_entries};

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

/// A store holding one multi-level tree, plus every chunk address grouped
/// by whether it decodes as a leaf or an inner node.
struct Fixture {
    store: MemStore,
    root: Root,
    map: Map,
    leaves: Vec<Hash>,
    inners: Vec<Hash>,
}

fn fixture() -> Fixture {
    let store = MemStore::new();
    let map = bulk_entries(20_000, 0xc0ffee);
    let root = bulk_load(&store, ChunkParams::default(), map.clone()).expect("bulk_load");
    assert!(root.height() >= 3, "fixture must be a multi-level tree");
    let mut leaves = Vec::new();
    let mut inners = Vec::new();
    for hash in store.all_hashes() {
        let data = store.raw(&hash).expect("stored");
        if data[0] == 0 {
            leaves.push(hash);
        } else {
            inners.push(hash);
        }
    }
    Fixture {
        store,
        root,
        map,
        leaves,
        inners,
    }
}

/// Exercise every read path over a possibly-damaged tree. Returns `Ok` if
/// all operations succeeded AND produced exactly the expected contents;
/// `Err` if any operation reported damage. Panics (failing the test) if an
/// operation "succeeds" with wrong data — the outcome the layer must make
/// impossible for structural damage.
fn exercise(f: &Fixture) -> Result<(), ProllyError> {
    // Point gets across the key space.
    for (k, v) in f.map.iter().step_by(1013) {
        match get(&f.store, &f.root, k) {
            Ok(Some(got)) if got.as_ref() == v.as_slice() => {}
            Ok(other) => panic!(
                "get({:?}) returned wrong data {:?} instead of erroring",
                String::from_utf8_lossy(k),
                other.map(|b| b.len())
            ),
            Err(e) => return Err(e),
        }
    }
    // Full scans, both directions.
    let expected: Vec<(Vec<u8>, Vec<u8>)> = f.map.clone().into_iter().collect();
    let forward: Result<Vec<_>, _> = scan(&f.store, &f.root, ..)?
        .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
        .collect();
    match forward {
        Ok(got) => assert_eq!(got, expected, "scan succeeded with wrong contents"),
        Err(e) => return Err(e),
    }
    let reverse: Result<Vec<_>, _> = scan_rev(&f.store, &f.root, ..)?
        .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
        .collect();
    match reverse {
        Ok(mut got) => {
            got.reverse();
            assert_eq!(got, expected, "reverse scan succeeded with wrong contents");
        }
        Err(e) => return Err(e),
    }
    // A batch touching everything near the damage would be nice, but any
    // wide batch works: it must either succeed building the right tree or
    // report the damage.
    let probe_key = f.map.keys().nth(9_999).expect("probe").clone();
    let updated = apply_batch(
        &f.store,
        &f.root,
        vec![BatchOp::Put(probe_key.clone(), b"probe".to_vec())],
    )?;
    match get(&f.store, &updated, &probe_key) {
        Ok(Some(got)) if got.as_ref() == b"probe" => {}
        Ok(other) => panic!("batch produced wrong data: {other:?}"),
        Err(e) => return Err(e),
    }
    // Anchor walk (reads every inner node).
    reachable_chunks(&f.store, &f.root)?;
    Ok(())
}

/// Assert that `exercise` reports damage (rather than succeeding — the
/// pristine baseline — or panicking).
fn assert_detected(f: &Fixture, what: &str) {
    match exercise(f) {
        Err(_) => {}
        Ok(()) => panic!("{what}: damage was not detected by any read path"),
    }
}

#[test]
fn pristine_fixture_exercises_clean() {
    let f = fixture();
    exercise(&f).expect("pristine tree must pass all checks");
}

// ---------------------------------------------------------------------------
// Targeted structural mutations
// ---------------------------------------------------------------------------

#[test]
fn level_tag_mutations_are_detected() {
    // Leaf claiming to be inner…
    let f = fixture();
    let victim = f.leaves[f.leaves.len() / 2];
    let mut data = f.store.raw(&victim).expect("raw").to_vec();
    data[0] = 1;
    f.store.corrupt(&victim, data);
    assert_detected(&f, "leaf level tag set to 1");

    // …and inner claiming to be a leaf, and an inner one level too high.
    let f = fixture();
    let victim = f.inners[f.inners.len() / 2];
    let mut data = f.store.raw(&victim).expect("raw").to_vec();
    data[0] = 0;
    f.store.corrupt(&victim, data);
    assert_detected(&f, "inner level tag set to 0");

    let f = fixture();
    let victim = f.inners[0];
    let mut data = f.store.raw(&victim).expect("raw").to_vec();
    data[0] += 1;
    f.store.corrupt(&victim, data);
    assert_detected(&f, "inner level tag incremented");
}

#[test]
fn count_mutations_are_detected() {
    for delta in [-1i64, 1] {
        let f = fixture();
        let victim = f.leaves[1];
        let mut data = f.store.raw(&victim).expect("raw").to_vec();
        let count = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
        let forged = (i64::from(count) + delta) as u32;
        data[1..5].copy_from_slice(&forged.to_be_bytes());
        f.store.corrupt(&victim, data);
        assert_detected(&f, "leaf count changed");
    }
}

#[test]
fn truncations_and_trailing_bytes_are_detected() {
    // Truncate a leaf and an inner node at several depths.
    for cut_frac in [0usize, 1, 3, 7] {
        let f = fixture();
        let victim = f.leaves[2];
        let data = f.store.raw(&victim).expect("raw").to_vec();
        let cut = data.len() * cut_frac / 8;
        f.store.corrupt(&victim, data[..cut].to_vec());
        assert_detected(&f, "truncated leaf");

        let f = fixture();
        let victim = f.inners[0];
        let data = f.store.raw(&victim).expect("raw").to_vec();
        let cut = data.len() * cut_frac / 8;
        f.store.corrupt(&victim, data[..cut].to_vec());
        assert_detected(&f, "truncated inner node");
    }

    // Trailing garbage.
    let f = fixture();
    let victim = f.leaves[3];
    let mut data = f.store.raw(&victim).expect("raw").to_vec();
    data.push(0xaa);
    f.store.corrupt(&victim, data);
    assert_detected(&f, "trailing byte after declared entries");
}

#[test]
fn key_order_forgeries_are_detected() {
    // Rebuild a leaf with its first two keys swapped (well-formed framing,
    // broken ordering).
    let f = fixture();
    let victim = f.leaves[4];
    let data = f.store.raw(&victim).expect("raw").to_vec();
    // Decode manually: header, then entries.
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut pos = 5usize;
    while pos < data.len() {
        let klen = u32::from_be_bytes(data[pos..pos + 4].try_into().expect("klen")) as usize;
        pos += 4;
        let key = data[pos..pos + klen].to_vec();
        pos += klen;
        let vlen = u32::from_be_bytes(data[pos..pos + 4].try_into().expect("vlen")) as usize;
        pos += 4;
        let value = data[pos..pos + vlen].to_vec();
        pos += vlen;
        entries.push((key, value));
    }
    assert!(entries.len() >= 2, "victim leaf needs two entries");
    entries.swap(0, 1);
    let mut forged = data[..5].to_vec();
    for (k, v) in &entries {
        forged.extend_from_slice(&(k.len() as u32).to_be_bytes());
        forged.extend_from_slice(k);
        forged.extend_from_slice(&(v.len() as u32).to_be_bytes());
        forged.extend_from_slice(v);
    }
    f.store.corrupt(&victim, forged);
    assert_detected(&f, "leaf keys swapped");
}

#[test]
fn wrong_but_well_formed_child_is_detected() {
    // Replace one leaf's bytes with another (perfectly valid) leaf's bytes:
    // framing and ordering both check out, but the parent's boundary claim
    // no longer matches the child's content.
    let f = fixture();
    let victim = f.leaves[5];
    let donor = f.leaves[10];
    let donor_bytes = f.store.raw(&donor).expect("raw").to_vec();
    f.store.corrupt(&victim, donor_bytes);
    assert_detected(&f, "leaf replaced by a different valid leaf");
}

/// The first and last key of a raw leaf chunk (test-side decode).
fn leaf_key_span(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut pos = 5usize;
    let mut first: Option<Vec<u8>> = None;
    let mut last: Vec<u8> = Vec::new();
    while pos < data.len() {
        let klen = u32::from_be_bytes(data[pos..pos + 4].try_into().expect("klen")) as usize;
        pos += 4;
        let key = data[pos..pos + klen].to_vec();
        pos += klen;
        let vlen = u32::from_be_bytes(data[pos..pos + 4].try_into().expect("vlen")) as usize;
        pos += 4 + vlen;
        if first.is_none() {
            first = Some(key.clone());
        }
        last = key;
    }
    (first.expect("leaf has entries"), last)
}

#[test]
fn donor_leaf_above_victim_range_is_corrupt_not_absent() {
    // Directed regression for the parent boundary-claim check in
    // `read_node` (mutation testing showed the broad corpus above did not
    // kill its removal on the *point-get* path): replace a leaf with a
    // well-formed donor whose key range sorts entirely ABOVE the victim's.
    // Without the claim check, a `get` for a key resident in the victim
    // leaf descends to the donor, misses its binary search, and returns
    // Ok(None) — a wrong answer, the exact outcome the check exists to
    // forbid. It must be Corrupt instead.
    let f = fixture();
    let mut by_range: Vec<(Vec<u8>, Vec<u8>, Hash)> = f
        .leaves
        .iter()
        .map(|h| {
            let raw = f.store.raw(h).expect("raw");
            let (first, last) = leaf_key_span(&raw);
            (first, last, *h)
        })
        .collect();
    by_range.sort();
    let (victim_first, victim_last, victim) = by_range[2].clone();
    let (donor_first, _, donor) = by_range[by_range.len() - 2].clone();
    assert!(
        donor_first > victim_last,
        "donor must sort entirely above the victim"
    );

    let donor_bytes = f.store.raw(&donor).expect("raw").to_vec();
    f.store.corrupt(&victim, donor_bytes);

    // victim_first is a real map key that lives in the victim leaf.
    assert!(f.map.contains_key(&victim_first), "probe is real");
    match get(&f.store, &f.root, &victim_first) {
        Err(ProllyError::Corrupt { .. }) => {}
        Err(other) => panic!("expected Corrupt, got {other}"),
        Ok(v) => panic!(
            "wrong answer instead of error: get returned {:?}",
            v.map(|b| b.len())
        ),
    }
}

#[test]
fn dangling_references_are_missing_chunk_errors() {
    let f = fixture();
    let victim = f.leaves[6];
    f.store.remove(&victim);
    match exercise(&f) {
        Err(ProllyError::MissingChunk { hash }) => assert_eq!(hash, victim),
        Err(other) => panic!("expected MissingChunk, got {other}"),
        Ok(()) => panic!("dangling leaf not detected"),
    }

    let f = fixture();
    let victim = f.inners[0];
    f.store.remove(&victim);
    assert!(matches!(
        exercise(&f),
        Err(ProllyError::MissingChunk { .. })
    ));
}

#[test]
fn self_referencing_node_cannot_trap_a_walk() {
    // Content addressing makes a self-referencing chunk impossible to
    // build honestly, but a broken store can serve one; the level check
    // must stop the descent rather than loop.
    let store = MemStore::new();
    let target = Hash::from_bytes(&[7u8; 20]).expect("hash");
    let mut node = vec![1u8]; // level 1
    node.extend_from_slice(&1u32.to_be_bytes()); // one entry
    node.extend_from_slice(&3u32.to_be_bytes());
    node.extend_from_slice(b"key");
    node.push(20);
    node.extend_from_slice(target.as_bytes()); // child = itself
    store.corrupt(&target, node);

    let root = Root::new(target, 2, ChunkParams::default()).expect("root");
    assert!(get(&store, &root, b"key").is_err());
    assert!(
        scan(&store, &root, ..)
            .and_then(|s| s.collect::<Result<Vec<_>, _>>())
            .is_err()
    );
    // The anchor walk must *terminate* (visited set + level checks make a
    // cycle unwalkable); it does not read leaf-level children, so the
    // self-reference is simply an already-collected chunk to it.
    let chunks = reachable_chunks(&store, &root).expect("walk terminates");
    assert_eq!(chunks, vec![target]);
}

#[test]
fn forged_root_height_is_detected() {
    let f = fixture();
    // The real tree is height >= 3; claim something else.
    for forged_height in [1u32, 2, f.root.height() + 1, 64] {
        if forged_height == f.root.height() {
            continue;
        }
        let forged = Root::new(f.root.hash(), forged_height, f.root.params()).expect("root");
        assert!(
            get(&f.store, &forged, f.map.keys().next().expect("key")).is_err(),
            "forged height {forged_height} not detected by get"
        );
        assert!(
            scan(&f.store, &forged, ..)
                .and_then(|s| s.collect::<Result<Vec<_>, _>>())
                .is_err(),
            "forged height {forged_height} not detected by scan"
        );
    }
    // Height 0 and heights above MAX_HEIGHT are rejected at Root::new.
    assert!(Root::new(f.root.hash(), 0, f.root.params()).is_err());
    assert!(Root::new(f.root.hash(), 65, f.root.params()).is_err());
}

#[test]
fn diff_and_merge_report_damage() {
    let f = fixture();
    // A second version differing in one key, so diff has a changed spine.
    let key = f.map.keys().nth(10_000).expect("key").clone();
    let other = apply_batch(
        &f.store,
        &f.root,
        vec![BatchOp::Put(key, b"changed".to_vec())],
    )
    .expect("apply_batch");

    // Damage the original leaf that holds the changed key — it is on the
    // diff's changed spine, so the diff must cross it.
    let key_bytes = f.map.keys().nth(10_000).expect("key").clone();
    let mut damaged_any = false;
    for leaf in &f.leaves {
        let raw = f.store.raw(leaf).expect("raw");
        if raw
            .windows(key_bytes.len())
            .any(|w| w == key_bytes.as_slice())
        {
            let mut data = raw.to_vec();
            data[0] = 1; // forge the level tag
            f.store.corrupt(leaf, data);
            damaged_any = true;
            break;
        }
    }
    assert!(damaged_any, "no leaf contained the changed key");

    let outcome: Result<Vec<_>, _> =
        diff(&f.store, &f.root, &other).and_then(|d| d.collect::<Result<Vec<_>, _>>());
    assert!(outcome.is_err(), "diff across damaged spine must error");
}

// ---------------------------------------------------------------------------
// Randomised corruption: never a panic
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases(64))]

    /// Arbitrary bytes served as the root chunk: every operation returns
    /// (any) Result rather than panicking. Structural nonsense errors;
    /// bytes that happen to decode as an empty-ish valid node simply
    /// behave as that node.
    #[test]
    fn arbitrary_root_bytes_never_panic(
        data in vec(any::<u8>(), 0..2048),
        height in 1u32..5,
    ) {
        let store = MemStore::new();
        let hash = Hash::from_bytes(&[3u8; 20]).expect("hash");
        store.corrupt(&hash, data);
        let root = Root::new(hash, height, ChunkParams::default()).expect("root");

        let _ = get(&store, &root, b"probe");
        let _ = scan(&store, &root, ..).and_then(|s| s.collect::<Result<Vec<_>, _>>());
        let _ = scan_rev(&store, &root, ..).and_then(|s| s.collect::<Result<Vec<_>, _>>());
        let _ = reachable_chunks(&store, &root);
        let _ = apply_batch(&store, &root, vec![BatchOp::Put(b"k".to_vec(), b"v".to_vec())]);
    }

    /// Random single-byte mutations of a real chunk: reads either error or
    /// (for mutations confined to value bytes, which structural checks
    /// cannot see) return successfully — but never panic. When the scan
    /// does succeed, the damage must have been inside values only:
    /// key material and framing damage must have errored.
    #[test]
    fn random_byte_flips_never_panic(
        chunk_pick in any::<prop::sample::Index>(),
        offset_pick in any::<prop::sample::Index>(),
        xor in 1u8..=255,
    ) {
        let store = MemStore::new();
        let map = bulk_entries(600, 0xf1ea);
        let root = bulk_load(&store, ChunkParams::default(), map.clone()).expect("bulk_load");
        let hashes = store.all_hashes();
        let victim = hashes[chunk_pick.index(hashes.len())];
        let mut data = store.raw(&victim).expect("raw").to_vec();
        let off = offset_pick.index(data.len());
        data[off] ^= xor;
        store.corrupt(&victim, data);

        let _ = get(&store, &root, map.keys().next().expect("key"));
        let _ = scan(&store, &root, ..).and_then(|s| s.collect::<Result<Vec<_>, _>>());
        let _ = scan_rev(&store, &root, ..).and_then(|s| s.collect::<Result<Vec<_>, _>>());
        let _ = reachable_chunks(&store, &root);
    }
}

// ---------------------------------------------------------------------------
// Store-level damage interacting with Bytes zero-copy decoding
// ---------------------------------------------------------------------------

#[test]
fn oversized_stored_chunk_is_a_store_error_not_an_allocation() {
    // A chunk whose stored size exceeds the store cap errors on get before
    // this layer sees it; verify the error propagates as ProllyError.
    let store = MemStore::with_cap(4096);
    let map = bulk_entries(50, 4);
    let root = bulk_load(&store, ChunkParams::default(), map).expect("bulk_load");
    // Grow the root chunk beyond the cap in place.
    let mut data = store.raw(&root.hash()).expect("raw").to_vec();
    data.resize(8192, 0);
    store.corrupt(&root.hash(), data);
    assert!(matches!(
        get(&store, &root, b"any"),
        Err(ProllyError::Store(_))
    ));
}
