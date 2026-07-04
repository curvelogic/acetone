//! ChunkStore contract tests: round-trip, idempotence, absence vs damage,
//! and hard size-cap enforcement on both the write and read paths.

mod common;

use acetone_store::{ChunkStore, GitStore, Hash, StoreError};
use common::{new_store, repo_path};

#[test]
fn put_get_round_trip() {
    let (_dir, store) = new_store();
    let data = b"hello, chunk store".as_slice();
    let hash = store.put(data).expect("put");
    let back = store.get(&hash).expect("get").expect("present");
    assert_eq!(back.as_ref(), data);
}

#[test]
fn put_is_content_addressed_and_idempotent() {
    let (_dir, store) = new_store();
    let h1 = store.put(b"same bytes").expect("put");
    let h2 = store.put(b"same bytes").expect("put");
    assert_eq!(h1, h2, "identical bytes must yield identical hashes");
    let h3 = store.put(b"different bytes").expect("put");
    assert_ne!(h1, h3, "different bytes must yield different hashes");
}

#[test]
fn empty_chunk_round_trips() {
    let (_dir, store) = new_store();
    let hash = store.put(b"").expect("put empty");
    let back = store.get(&hash).expect("get").expect("present");
    assert!(back.is_empty());
}

#[test]
fn get_absent_is_none_not_error() {
    let (_dir, store) = new_store();
    // A syntactically valid hash that addresses nothing.
    let absent = Hash::from_hex("0123456789abcdef0123456789abcdef01234567").expect("hash");
    assert!(store.get(&absent).expect("get").is_none());
}

#[test]
fn put_batch_matches_individual_puts() {
    let (_dir, store) = new_store();
    let chunks: Vec<Vec<u8>> = (0..64u32)
        .map(|i| format!("chunk-{i}").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
    let batch_hashes = store.put_batch(&refs).expect("put_batch");
    assert_eq!(batch_hashes.len(), chunks.len());
    for (chunk, hash) in chunks.iter().zip(&batch_hashes) {
        assert_eq!(store.put(chunk).expect("put"), *hash);
        assert_eq!(
            store.get(hash).expect("get").expect("present").as_ref(),
            chunk.as_slice()
        );
    }
}

#[test]
fn put_rejects_oversized_and_accepts_exact_cap() {
    let (_dir, store) = common::new_capped_store(1024);
    assert_eq!(store.max_chunk_size(), 1024);

    // Exactly at the cap: accepted, round-trips.
    let exact = vec![0xabu8; 1024];
    let hash = store.put(&exact).expect("put at cap");
    assert_eq!(
        store.get(&hash).expect("get").expect("present").as_ref(),
        exact.as_slice()
    );

    // One byte over: rejected before any write.
    let over = vec![0xabu8; 1025];
    match store.put(&over) {
        Err(StoreError::ObjectTooLarge { size, limit }) => {
            assert_eq!(size, 1025);
            assert_eq!(limit, 1024);
        }
        other => panic!("expected ObjectTooLarge, got {other:?}"),
    }
}

#[test]
fn get_rejects_oversized_object_before_materialising() {
    // A hostile repository can contain arbitrarily large blobs regardless
    // of what our own put() would accept: write one with a permissive
    // store, then read the same repository through a capped store.
    let (dir, writer) = new_store();
    let big = vec![0x5au8; 512 * 1024];
    let hash = writer.put(&big).expect("put big");
    drop(writer);

    let capped =
        GitStore::open_with(&repo_path(&dir), common::capped_options(4096)).expect("open capped");
    match capped.get(&hash) {
        Err(StoreError::ObjectTooLarge { size, limit }) => {
            assert_eq!(size, 512 * 1024);
            assert_eq!(limit, 4096);
        }
        other => panic!("expected ObjectTooLarge, got {other:?}"),
    }
}

#[test]
fn get_on_non_blob_object_is_error_not_none() {
    // Distinguishing damage from absence: a present-but-wrong-kind object
    // must be Err, never Ok(None). The empty tree exists in every repo
    // once written.
    let (dir, store) = new_store();
    let tree_hex = common::git_stdin(&repo_path(&dir), &["mktree"], b"");
    let tree_hash = Hash::from_hex(tree_hex.trim()).expect("hash");
    match store.get(&tree_hash) {
        Err(StoreError::WrongObjectKind { expected, .. }) => assert_eq!(expected, "blob"),
        other => panic!("expected WrongObjectKind, got {other:?}"),
    }
}

#[test]
fn corrupt_loose_object_is_error_not_panic() {
    let (dir, store) = new_store();
    let hash = store.put(b"soon to be corrupted").expect("put");
    drop(store);

    // Overwrite the loose object file with garbage (loose objects are
    // read-only, so make it writable first).
    let hex = hash.to_hex();
    let object_path = repo_path(&dir)
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    let mut perms = std::fs::metadata(&object_path).expect("stat").permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&object_path, perms).expect("chmod");
    std::fs::write(&object_path, b"this is not zlib data").expect("corrupt object");

    let store = GitStore::open(&repo_path(&dir)).expect("reopen");
    let result = store.get(&hash);
    assert!(
        result.is_err(),
        "corrupt object must surface as an error, got {result:?}"
    );
}

#[test]
fn hash_hex_and_bytes_round_trip() {
    let (_dir, store) = new_store();
    let hash = store.put(b"addressable").expect("put");

    let hex = hash.to_hex();
    assert_eq!(Hash::from_hex(&hex).expect("from_hex"), hash);
    assert_eq!(format!("{hash}"), hex);

    let bytes = hash.as_bytes().to_vec();
    assert_eq!(Hash::from_bytes(&bytes).expect("from_bytes"), hash);

    assert!(matches!(
        Hash::from_hex("not hex at all"),
        Err(StoreError::InvalidHash { .. })
    ));
    assert!(matches!(
        Hash::from_hex("abc123"),
        Err(StoreError::InvalidHash { .. })
    ));
    assert!(matches!(
        Hash::from_bytes(&[1, 2, 3]),
        Err(StoreError::InvalidHash { .. })
    ));
}
