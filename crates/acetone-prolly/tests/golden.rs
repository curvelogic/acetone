//! Golden byte- and root-hash pins for the prolly on-disk format
//! (format_version 1; Gate D freeze, ADR-0024).
//!
//! Every other on-disk artefact — keys, values, records, schema, manifest —
//! is byte-pinned in `acetone-model`'s golden suite. The prolly node framing
//! (`level:u8 count:u32 entry*`, big-endian; see `node.rs`) and the
//! content-defined chunker were, until this suite, guarded only by
//! *self-relative* property tests (`apply_batch == bulk_load`, revert
//! restores, cross-store equality). Those pass equally on a *changed* node
//! header, entry framing, integer width, or chunker boundary — as long as the
//! change is internally consistent — so a format drift would silently alter
//! **every root hash** while `format_version` stayed 1. That is exactly the
//! two-builds-disagree-under-one-version failure a freeze exists to prevent.
//!
//! This suite closes the gap. It pins, for **two** chunk profiles:
//!
//! - a leaf chunk's exact bytes (a single-leaf map),
//! - an inner chunk's exact bytes (a map tall enough to have an inner root),
//! - and the **root hash** of each — a content address that transitively
//!   fixes every byte of every reachable node plus the chunker boundaries.
//!
//! The two profiles (ADR-0045, acetone-7bn.18):
//!
//! - **`ChunkParams::default()`** = `(1024, 12, 16384)` — the Phase-0
//!   spike/test profile, max 16 KiB. This is NOT what a real repository uses,
//!   but it is the benchmark regression baseline, and its 16 KiB ceiling forces
//!   an inner root from a smaller dataset than the shipped profile does.
//! - **The shipped repository profile** = `(1024, 12, 65536)`, max 64 KiB —
//!   what `acetone init` writes via `acetone_graph::repo::default_chunk_params()`.
//!   Pinned here explicitly (`shipped_chunk_params()`) so the byte-exact goldens
//!   cover the profile every real repository actually produces. A guard test in
//!   `acetone-graph` (`repository.rs`) pins `default_chunk_params()` to these
//!   same values, so drift in one forces reconciliation with the other.
//!
//! Under the shipped profile a sub-ceiling map chunks identically to the
//! default profile (`max_bytes` only bounds *large* chunks), so the two
//! profiles diverge exactly where a chunk would cross 16 KiB — which is why the
//! inner-node goldens use different dataset sizes and produce different trees.
//!
//! Addresses are git blob hashes (SHA-1), the same a `GitStore` assigns, so a
//! pin here is the on-disk identity a real repository would produce.
//!
//! **Do not "fix" a failing pin by updating the expected bytes.** A change
//! here means the on-disk format moved: bump `manifest::FORMAT_VERSION`,
//! provide an `acetone migrate`, and re-pin deliberately.

mod common;

use acetone_prolly::{BatchOp, ChunkParams, Root, apply_batch, empty};
use common::MemStore;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The chunk profile a real repository uses — `acetone init` writes exactly
/// these values via `acetone_graph::repo::default_chunk_params()` (mirrored
/// here because `acetone-prolly` sits below `acetone-graph` and cannot depend
/// on it; a guard test in `acetone-graph`'s `repository.rs` keeps the two in
/// lock-step). Distinct from `ChunkParams::default()`'s 16 KiB spike profile.
fn shipped_chunk_params() -> ChunkParams {
    ChunkParams::new(1024, 12, 65536).expect("shipped parameters are valid")
}

/// Build a map from `(key, value)` pairs under `params`.
fn build_with(store: &MemStore, entries: &[(Vec<u8>, Vec<u8>)], params: ChunkParams) -> Root {
    let root = empty(store, params).expect("empty");
    let ops: Vec<BatchOp> = entries
        .iter()
        .map(|(k, v)| BatchOp::Put(k.clone(), v.clone()))
        .collect();
    apply_batch(store, &root, ops).expect("apply_batch")
}

/// Build a map under the default (spike/test) chunk parameters.
fn build(store: &MemStore, entries: &[(Vec<u8>, Vec<u8>)]) -> Root {
    build_with(store, entries, ChunkParams::default())
}

/// The default-profile single-leaf root hash (pinned in
/// `golden_leaf_node_and_root_hash`). A sub-ceiling map chunks the same under
/// both profiles, so the shipped-profile leaf test asserts against this.
const DEFAULT_LEAF_ROOT_HASH: &str = "c0df7111303ecded3ed7d1aee17379c3e8eca559";

/// A single-leaf map: two small entries fit comfortably below one chunk
/// boundary, so the root *is* a leaf. Pins the leaf framing and its address.
#[test]
fn golden_leaf_node_and_root_hash() {
    let store = MemStore::new();
    let entries = vec![
        (b"key1".to_vec(), b"val1".to_vec()),
        (b"key2".to_vec(), b"val2".to_vec()),
    ];
    let root = build(&store, &entries);
    assert_eq!(root.height(), 1, "two small entries form a single leaf");

    let root_bytes = store.raw(&root.hash()).expect("root chunk present");
    // level 0 (leaf), count 2, then (klen,key,vlen,value) per entry.
    assert_eq!(
        hex(&root_bytes),
        "0000000002000000046b6579310000000476616c31000000046b6579320000000476616c32",
        "leaf node on-disk framing (format_version 1)"
    );
    assert_eq!(
        root.hash().to_hex(),
        "c0df7111303ecded3ed7d1aee17379c3e8eca559",
        "leaf-map root hash (format_version 1)"
    );
}

/// A map tall enough (height ≥ 2) that its root is an **inner** node, under
/// the default chunk parameters. Pins the inner framing
/// (`klen,last_key,hlen,hash`) and the root address, which transitively fixes
/// the whole tree and the chunker boundaries.
#[test]
fn golden_inner_node_and_root_hash() {
    let store = MemStore::new();
    // Enough fixed entries to cross several default-parameter chunk
    // boundaries and force an inner root. Deterministic content.
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..600u32)
        .map(|i| {
            let key = format!("node/{i:06}").into_bytes();
            let value = format!("value-{i:06}-{}", "x".repeat(48)).into_bytes();
            (key, value)
        })
        .collect();
    let root = build(&store, &entries);
    assert!(
        root.height() >= 2,
        "600 padded entries must form a tree of height >= 2 (got {})",
        root.height()
    );

    let root_bytes = store.raw(&root.hash()).expect("root chunk present");
    assert_eq!(
        hex(&root_bytes),
        "01000000030000000b6e6f64652f303030323034141996f92f478c6a65473fc63bc75e1ec85daf6c530000000b6e6f64652f303030343039145ff806dc4f0107b1ec457bd11ad4bd3f3cfbf9420000000b6e6f64652f30303035393914dbeb42412687471e07aa8fd336859cc4afbc9f29",
        "inner node on-disk framing (format_version 1)"
    );
    assert_eq!(
        root.hash().to_hex(),
        "3164ef68a5cc86ba5a2e765d74e1864c8dd3ca1e",
        "inner-map root hash (format_version 1)"
    );
    assert_eq!(root.height(), 2, "inner-map tree height (format_version 1)");
}

/// A sub-ceiling map chunks identically under both profiles — `max_bytes`
/// only forces a cut on chunks that reach it, and two tiny entries never do.
/// This pins that invariant: the shipped 64 KiB profile produces the *same*
/// single-leaf root as the default 16 KiB profile for a small map, so the leaf
/// framing golden above already covers the shipped profile in the small case.
#[test]
fn golden_shipped_profile_small_map_matches_default_profile() {
    let entries = vec![
        (b"key1".to_vec(), b"val1".to_vec()),
        (b"key2".to_vec(), b"val2".to_vec()),
    ];
    let shipped_store = MemStore::new();
    let shipped = build_with(&shipped_store, &entries, shipped_chunk_params());
    assert_eq!(shipped.height(), 1, "two small entries form a single leaf");
    assert_eq!(
        shipped.hash().to_hex(),
        DEFAULT_LEAF_ROOT_HASH,
        "sub-ceiling map is profile-independent (format_version 1)"
    );
}

/// The **shipped repository profile**'s inner node and root hash — the
/// on-disk identity a real `acetone init`'d repository produces. The 64 KiB
/// ceiling holds far more entries per leaf than the default 16 KiB profile, so
/// forcing a height ≥ 2 tree needs a larger dataset (2000 padded entries here,
/// vs 600 for the default profile); the resulting tree is genuinely different,
/// which is the whole point of pinning both. Byte-exact under format_version 1.
#[test]
fn golden_shipped_profile_inner_node_and_root_hash() {
    let store = MemStore::new();
    // Enough fixed entries to cross several 64 KiB-profile chunk boundaries and
    // force an inner root. Deterministic content, same shape as the default-
    // profile inner golden (only the count differs).
    let entries: Vec<(Vec<u8>, Vec<u8>)> = (0..2000u32)
        .map(|i| {
            let key = format!("node/{i:06}").into_bytes();
            let value = format!("value-{i:06}-{}", "x".repeat(48)).into_bytes();
            (key, value)
        })
        .collect();
    let root = build_with(&store, &entries, shipped_chunk_params());
    assert!(
        root.height() >= 2,
        "2000 padded entries must form a tree of height >= 2 under the shipped \
         profile (got {})",
        root.height()
    );

    let root_bytes = store.raw(&root.hash()).expect("root chunk present");
    assert_eq!(
        hex(&root_bytes),
        "01000000030000000b6e6f64652f30303038313914c68e69ff460deadcedfb879d616d14ade537d6d90000000b6e6f64652f30303136333914e8d55a34f79545d7fb1f98fd540b4895d18ce0580000000b6e6f64652f30303139393914c81604c4573e1b1a00e64c6185166d7d2319436a",
        "shipped-profile inner node on-disk framing (format_version 1)"
    );
    assert_eq!(
        root.hash().to_hex(),
        "a7a7ad7f1384da3c23e651e9ac0b60d873fa5c3c",
        "shipped-profile inner-map root hash (format_version 1)"
    );
    assert_eq!(
        root.height(),
        2,
        "shipped-profile inner-map tree height (format_version 1)"
    );
}
