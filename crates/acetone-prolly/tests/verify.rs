//! `verify_reachable`: the integrity walk that backs `fsck`.
//!
//! Every case here asserts the verifier's two load-bearing properties:
//! a healthy tree yields **no** faults, and any structural damage yields a
//! fault that is *classified correctly* (MISSING vs CORRUPT) and *names the
//! offending chunk* — never a panic and never a false "clean". These
//! mirror the read-path corpus in `corruption.rs`, but assert the
//! structured, non-aborting report `fsck` consumes rather than a single
//! propagated error.

mod common;

use proptest::prelude::*;

use acetone_prolly::{
    BatchOp, ChunkFaultKind, ChunkParams, Hash, Root, apply_batch, bulk_load, empty, get,
    reachable_chunks, scan, verify_reachable,
};
use acetone_store::ChunkStore;
use common::{Map, MemStore, bulk_entries};

/// Encode a leaf chunk (level 0) from `(key, value)` entries, in the node
/// format, so tests can hand-build specific — including deliberately
/// misplaced — trees whose chunks are honestly content-addressed.
fn leaf(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut out = vec![0u8];
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (k, v) in entries {
        out.extend_from_slice(&(k.len() as u32).to_be_bytes());
        out.extend_from_slice(k);
        out.extend_from_slice(&(v.len() as u32).to_be_bytes());
        out.extend_from_slice(v);
    }
    out
}

/// Encode an inner chunk at `level` from `(last_key, child hash)` refs.
fn inner(level: u8, refs: &[(&[u8], Hash)]) -> Vec<u8> {
    let mut out = vec![level];
    out.extend_from_slice(&(refs.len() as u32).to_be_bytes());
    for (k, h) in refs {
        out.extend_from_slice(&(k.len() as u32).to_be_bytes());
        out.extend_from_slice(k);
        let bytes = h.as_bytes();
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
    }
    out
}

/// A store holding one multi-level tree, with its chunk addresses grouped
/// into leaves (level tag 0) and inner nodes.
struct Fixture {
    store: MemStore,
    root: Root,
    leaves: Vec<Hash>,
    inners: Vec<Hash>,
}

fn fixture() -> Fixture {
    let store = MemStore::new();
    let map: Map = bulk_entries(20_000, 0xc0ffee);
    let root = bulk_load(&store, ChunkParams::default(), map).expect("bulk_load");
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
        leaves,
        inners,
    }
}

#[test]
fn pristine_tree_is_clean() {
    let f = fixture();
    assert_eq!(
        verify_reachable(&f.store, &f.root),
        Vec::new(),
        "a healthy tree must report no faults"
    );
}

#[test]
fn empty_tree_is_clean() {
    let store = MemStore::new();
    let root = empty(&store, ChunkParams::default()).expect("empty");
    assert!(verify_reachable(&store, &root).is_empty());
}

#[test]
fn apply_batch_tree_is_clean() {
    // History independence: apply_batch produces the same tree as a fresh
    // bulk_load, and the verifier must accept it.
    let store = MemStore::new();
    let base = empty(&store, ChunkParams::default()).expect("empty");
    let ops: Vec<BatchOp> = (0u32..8_000)
        .map(|i| BatchOp::Put(format!("k{i:07}").into_bytes(), vec![9u8; 48]))
        .collect();
    let root = apply_batch(&store, &base, ops).expect("apply_batch");
    assert!(root.height() >= 2);
    assert!(verify_reachable(&store, &root).is_empty());
}

#[test]
fn missing_root_is_one_missing_fault_naming_it() {
    let f = fixture();
    f.store.remove(&f.root.hash());
    let faults = verify_reachable(&f.store, &f.root);
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].kind, ChunkFaultKind::Missing);
    assert_eq!(faults[0].hash, f.root.hash());
}

#[test]
fn deleted_leaf_is_missing_and_named() {
    let f = fixture();
    let victim = f.leaves[f.leaves.len() / 2];
    f.store.remove(&victim);
    let faults = verify_reachable(&f.store, &f.root);
    assert!(
        faults
            .iter()
            .any(|fault| fault.hash == victim && fault.kind == ChunkFaultKind::Missing),
        "expected a MISSING fault naming {victim}, got {faults:?}"
    );
    assert!(
        faults
            .iter()
            .all(|fault| fault.kind == ChunkFaultKind::Missing),
        "a deletion must not manufacture corruption findings: {faults:?}"
    );
}

#[test]
fn deleted_inner_node_is_missing_and_hides_only_its_subtree() {
    let f = fixture();
    let victim = f.inners[0];
    f.store.remove(&victim);
    let faults = verify_reachable(&f.store, &f.root);
    // The inner node is reported; its descendants' addresses are unknown,
    // so no spurious findings are invented for them.
    assert!(
        faults
            .iter()
            .any(|fault| fault.hash == victim && fault.kind == ChunkFaultKind::Missing)
    );
    assert!(
        faults
            .iter()
            .all(|fault| fault.kind == ChunkFaultKind::Missing)
    );
}

#[test]
fn garbage_leaf_is_corrupt_and_named() {
    let f = fixture();
    let victim = f.leaves[3];
    f.store
        .corrupt(&victim, b"not a prolly node at all".to_vec());
    let faults = verify_reachable(&f.store, &f.root);
    assert_eq!(faults.len(), 1, "one corrupt leaf, one fault: {faults:?}");
    assert_eq!(faults[0].hash, victim);
    assert_eq!(faults[0].kind, ChunkFaultKind::Corrupt);
}

#[test]
fn corrupt_inner_node_is_corrupt_and_named() {
    let f = fixture();
    let victim = f.inners[f.inners.len() / 2];
    // Forge the level tag: a valid inner node claiming the wrong level.
    let mut data = f.store.raw(&victim).expect("raw").to_vec();
    data[0] = 0;
    f.store.corrupt(&victim, data);
    let faults = verify_reachable(&f.store, &f.root);
    assert!(
        faults
            .iter()
            .any(|fault| fault.hash == victim && fault.kind == ChunkFaultKind::Corrupt),
        "expected a CORRUPT fault naming {victim}, got {faults:?}"
    );
}

#[test]
fn wrong_but_well_formed_leaf_is_corrupt_via_boundary_claim() {
    // Replace one leaf with another perfectly valid leaf: framing and
    // ordering both check out, but the parent's last-key claim no longer
    // matches, so the walk must flag it corrupt, not accept it.
    let f = fixture();
    let victim = f.leaves[5];
    let donor = f.leaves[10];
    let donor_bytes = f.store.raw(&donor).expect("raw").to_vec();
    f.store.corrupt(&victim, donor_bytes);
    let faults = verify_reachable(&f.store, &f.root);
    assert!(
        faults
            .iter()
            .any(|fault| fault.hash == victim && fault.kind == ChunkFaultKind::Corrupt),
        "swapped-but-valid leaf must be corrupt: {faults:?}"
    );
}

#[test]
fn truncation_anywhere_is_corrupt_not_panic() {
    let original = {
        let f = fixture();
        f.store.raw(&f.leaves[2]).expect("raw").to_vec()
    };
    // Cut the victim leaf at a spread of lengths; every one must be a
    // corrupt fault and none may panic.
    for cut in [0usize, 1, 5, original.len() / 3, original.len() - 1] {
        let f = fixture();
        let victim = f.leaves[2];
        let bytes = f.store.raw(&victim).expect("raw").to_vec();
        let cut = cut.min(bytes.len().saturating_sub(1));
        f.store.corrupt(&victim, bytes[..cut].to_vec());
        let faults = verify_reachable(&f.store, &f.root);
        assert!(
            faults
                .iter()
                .any(|fault| fault.kind == ChunkFaultKind::Corrupt),
            "truncation to {cut} bytes must produce a corrupt fault"
        );
    }
}

#[test]
fn missing_and_corrupt_are_reported_distinctly_in_one_pass() {
    let f = fixture();
    let gone = f.leaves[1];
    let bad = f.leaves[f.leaves.len() - 2];
    assert_ne!(gone, bad);
    f.store.remove(&gone);
    f.store.corrupt(&bad, vec![0xff; 4]);
    let faults = verify_reachable(&f.store, &f.root);
    assert!(
        faults
            .iter()
            .any(|fault| fault.hash == gone && fault.kind == ChunkFaultKind::Missing),
        "missing chunk not reported distinctly: {faults:?}"
    );
    assert!(
        faults
            .iter()
            .any(|fault| fault.hash == bad && fault.kind == ChunkFaultKind::Corrupt),
        "corrupt chunk not reported distinctly: {faults:?}"
    );
}

#[test]
fn oversized_stored_chunk_reads_as_corrupt() {
    // A stored object above the store's cap surfaces as a store error from
    // get (present but unreadable); the walk classifies it as corruption,
    // never a propagated abort or a panic. Few enough entries that the
    // build stays under the cap, then grow a chunk past it in place — the
    // pattern used by corruption.rs.
    let store = MemStore::with_cap(4096);
    let map = bulk_entries(50, 7);
    let root = bulk_load(&store, ChunkParams::default(), map).expect("bulk_load");
    let victim = root.hash();
    let mut data = store.raw(&victim).expect("raw").to_vec();
    data.resize(8192, 0);
    store.corrupt(&victim, data);
    let faults = verify_reachable(&store, &root);
    assert!(
        faults
            .iter()
            .any(|fault| fault.hash == victim && fault.kind == ChunkFaultKind::Corrupt),
        "oversized chunk must read as corrupt: {faults:?}"
    );
}

#[test]
fn self_referencing_inner_node_cannot_trap_the_walk() {
    // A broken store serving a self-referencing node: the level check makes
    // the cycle unwalkable, so the walk terminates with a fault instead of
    // looping. (Content addressing makes this impossible to build honestly.)
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
    // Terminates (does not hang) and reports the level-mismatched child.
    let faults = verify_reachable(&store, &root);
    assert!(
        faults
            .iter()
            .any(|fault| fault.kind == ChunkFaultKind::Corrupt),
        "self-reference must surface as corruption: {faults:?}"
    );
}

#[test]
fn first_child_spine_bound_is_inherited() {
    // Regression for a proven false-clean: the exclusive lower bound must be
    // inherited down the FIRST-child spine, exactly as the read paths do
    // (tree::get / Descent::descend use `prev_last_key.or(min_key_exclusive)`).
    //
    // Tree (honestly content-addressed, no bit-rot):
    //   root L2 [("c", A1), ("zz", B1)]
    //   A1  L1 [("c", leaf["b","c"])]
    //   B1  L1 [("z", L0), ("zz", M0)]
    //   L0 = leaf["a","z"]      <- first child of B1, whose inherited lower
    //                              bound is "c" (B1 follows A1); "a" <= "c",
    //                              so L0 holds keys below its position.
    let store = MemStore::new();
    let leaf_bc = store
        .put(&leaf(&[(b"b", b"1"), (b"c", b"1")]))
        .expect("put");
    let l0 = store
        .put(&leaf(&[(b"a", b"1"), (b"z", b"1")]))
        .expect("put");
    let m0 = store.put(&leaf(&[(b"zz", b"1")])).expect("put");
    let a1 = store.put(&inner(1, &[(b"c", leaf_bc)])).expect("put");
    let b1 = store
        .put(&inner(1, &[(b"z", l0), (b"zz", m0)]))
        .expect("put");
    let root_hash = store
        .put(&inner(2, &[(b"c", a1), (b"zz", b1)]))
        .expect("put");
    let root = Root::new(root_hash, 3, ChunkParams::default()).expect("root");

    // The read paths reject this tree, so a clean verify would be a genuine
    // false-clean.
    assert!(
        get(&store, &root, b"d").is_err(),
        "get must reject the misplaced tree"
    );
    assert!(
        scan(&store, &root, ..)
            .and_then(|s| s.collect::<Result<Vec<_>, _>>())
            .is_err(),
        "scan must reject the misplaced tree"
    );

    let faults = verify_reachable(&store, &root);
    assert!(
        faults
            .iter()
            .any(|f| f.hash == l0 && f.kind == ChunkFaultKind::Corrupt),
        "verify must flag L0 as corrupt (keys below its position), got {faults:?}"
    );
}

#[test]
fn shared_subtree_diamond_is_caught_and_terminates() {
    // A hostile store serving the same inner node from two references with
    // incompatible last-key claims (impossible to build honestly; a broken
    // store can serve it). The walk must terminate and flag the disorder,
    // never loop or return clean.
    let store = MemStore::new();
    let leaf_x = store
        .put(&leaf(&[(b"a", b"1"), (b"m", b"1")]))
        .expect("put");
    let x = store.put(&inner(1, &[(b"m", leaf_x)])).expect("put"); // X.last = "m"
    // Root references X twice: once truthfully ("m"), once with a false
    // boundary claim ("z"). The second reference hits the cached path.
    let root_hash = store.put(&inner(2, &[(b"m", x), (b"z", x)])).expect("put");
    let root = Root::new(root_hash, 3, ChunkParams::default()).expect("root");

    let faults = verify_reachable(&store, &root);
    assert!(
        faults.iter().any(|f| f.kind == ChunkFaultKind::Corrupt),
        "a shared subtree with a false boundary claim must be flagged, got {faults:?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Arbitrary bytes served as the root chunk: the walk always returns a
    /// (possibly empty) fault list rather than panicking.
    #[test]
    fn arbitrary_root_bytes_never_panic(
        data in prop::collection::vec(any::<u8>(), 0..2048),
        height in 1u32..5,
    ) {
        let store = MemStore::new();
        let hash = Hash::from_bytes(&[3u8; 20]).expect("hash");
        store.corrupt(&hash, data);
        let root = Root::new(hash, height, ChunkParams::default()).expect("root");
        let _ = verify_reachable(&store, &root);
    }

    /// Single-byte flips of a real chunk never panic, and the walk always
    /// terminates with a fault list. A flip can land in three places: a
    /// value byte (invisible to structural checks — the tree stays valid),
    /// framing/key material (the chunk is CORRUPT), or a child-pointer hash
    /// inside an inner node (the pointer now names a non-existent chunk, so
    /// that child reads MISSING). All three are acceptable; the invariant
    /// under test is that the walk does not hang or panic.
    #[test]
    fn random_byte_flips_never_panic(
        chunk_pick in any::<prop::sample::Index>(),
        offset_pick in any::<prop::sample::Index>(),
        xor in 1u8..=255,
    ) {
        let store = MemStore::new();
        let map = bulk_entries(600, 0xf1ea);
        let root = bulk_load(&store, ChunkParams::default(), map).expect("bulk_load");
        let hashes = store.all_hashes();
        let victim = hashes[chunk_pick.index(hashes.len())];
        let mut data = store.raw(&victim).expect("raw").to_vec();
        let off = offset_pick.index(data.len());
        data[off] ^= xor;
        store.corrupt(&victim, data);

        // Returns (does not hang/panic); the fault list is well-formed.
        let _faults = verify_reachable(&store, &root);
    }
}

/// The verifier reads leaves that the anchoring walk only enumerates by
/// address: a corrupt leaf slips past `reachable_chunks` but not past
/// `verify_reachable`. This is the false-clean hole a mere existence check
/// would leave.
#[test]
fn verifier_reads_leaves_that_anchoring_only_addresses() {
    let f = fixture();
    let victim = f.leaves[7];
    // reachable_chunks still lists the address (it comes from the parent),
    // so an existence-only check would pass.
    let anchored = reachable_chunks(&f.store, &f.root).expect("walk");
    assert!(anchored.contains(&victim));
    // Corrupt the leaf's *bytes*. Anchoring never reads them; verify does.
    f.store.corrupt(&victim, b"garbage".to_vec());
    let still_anchored = reachable_chunks(&f.store, &f.root).expect("walk");
    assert!(
        still_anchored.contains(&victim),
        "anchoring only addresses leaves, so it still lists the corrupt one"
    );
    let faults = verify_reachable(&f.store, &f.root);
    assert!(
        faults
            .iter()
            .any(|fault| fault.hash == victim && fault.kind == ChunkFaultKind::Corrupt),
        "verify must read the corrupt leaf the anchor walk skipped: {faults:?}"
    );
}
