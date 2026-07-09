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
//! This suite closes the gap. It pins, under the **default** chunk
//! parameters (`ChunkParams::default()` — the released format's defaults):
//!
//! - a leaf chunk's exact bytes (a single-leaf map),
//! - an inner chunk's exact bytes (a map tall enough to have an inner root),
//! - and the **root hash** of each — a content address that transitively
//!   fixes every byte of every reachable node plus the chunker boundaries.
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

/// Build a map from `(key, value)` pairs under the default chunk parameters.
fn build(store: &MemStore, entries: &[(Vec<u8>, Vec<u8>)]) -> Root {
    let root = empty(store, ChunkParams::default()).expect("empty");
    let ops: Vec<BatchOp> = entries
        .iter()
        .map(|(k, v)| BatchOp::Put(k.clone(), v.clone()))
        .collect();
    apply_batch(store, &root, ops).expect("apply_batch")
}

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
