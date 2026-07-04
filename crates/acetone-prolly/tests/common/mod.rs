//! Shared helpers for the acetone-prolly integration tests.
//!
//! Each integration test binary compiles this module separately and uses a
//! different subset of it, so unused-by-one-binary is expected.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use acetone_store::{Bytes, ChunkStore, Hash, StoreError};

// ---------------------------------------------------------------------------
// In-memory ChunkStore
// ---------------------------------------------------------------------------

/// An in-memory [`ChunkStore`] for tests.
///
/// Content-addressed with **git blob hashes** (SHA-1 by default), so a
/// chunk stored here has the same address it would have in a `GitStore` —
/// which is what makes cross-store determinism testable. Counts reads and
/// writes so tests can assert how much of the tree an operation touched,
/// and supports injecting corrupt bytes under an existing address to
/// simulate storage damage (something a real content-addressed store only
/// exhibits after bit rot, but which the decode paths must survive).
#[derive(Debug)]
pub struct MemStore {
    chunks: RefCell<BTreeMap<Hash, Bytes>>,
    reads: Cell<u64>,
    writes: Cell<u64>,
    max_chunk_size: u64,
}

impl Default for MemStore {
    fn default() -> Self {
        MemStore {
            chunks: RefCell::new(BTreeMap::new()),
            reads: Cell::new(0),
            writes: Cell::new(0),
            max_chunk_size: 64 * 1024 * 1024,
        }
    }
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store with a non-default object-size cap.
    pub fn with_cap(max_chunk_size: u64) -> Self {
        MemStore {
            max_chunk_size,
            ..Self::default()
        }
    }

    /// Chunk fetches so far (cheap observability for O(touched) claims).
    pub fn reads(&self) -> u64 {
        self.reads.get()
    }

    /// Chunk stores so far, including idempotent re-puts.
    pub fn writes(&self) -> u64 {
        self.writes.get()
    }

    pub fn reset_counters(&self) {
        self.reads.set(0);
        self.writes.set(0);
    }

    /// Number of distinct chunks held.
    pub fn len(&self) -> usize {
        self.chunks.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.borrow().is_empty()
    }

    /// Every address currently stored, sorted.
    pub fn all_hashes(&self) -> Vec<Hash> {
        self.chunks.borrow().keys().copied().collect()
    }

    /// The stored bytes for `hash`, if any (test observability).
    pub fn raw(&self, hash: &Hash) -> Option<Bytes> {
        self.chunks.borrow().get(hash).cloned()
    }

    /// Replace the bytes stored under `hash` **without** re-addressing
    /// them: simulates on-disk corruption of an existing object.
    pub fn corrupt(&self, hash: &Hash, data: Vec<u8>) {
        self.chunks.borrow_mut().insert(*hash, Bytes::from(data));
    }

    /// Remove the chunk at `hash`: simulates a gc'd / untransferred chunk.
    pub fn remove(&self, hash: &Hash) {
        self.chunks.borrow_mut().remove(hash);
    }

    /// The git blob address of `data` (what `put` would return).
    pub fn address(data: &[u8]) -> Hash {
        let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, data)
            .expect("SHA-1 blob hashing is infallible for in-memory data");
        Hash::from_bytes(oid.as_bytes()).expect("git digest is a valid hash width")
    }
}

impl ChunkStore for MemStore {
    fn put(&self, data: &[u8]) -> Result<Hash, StoreError> {
        self.writes.set(self.writes.get() + 1);
        if data.len() as u64 > self.max_chunk_size {
            return Err(StoreError::ObjectTooLarge {
                size: data.len() as u64,
                limit: self.max_chunk_size,
            });
        }
        let hash = Self::address(data);
        self.chunks
            .borrow_mut()
            .entry(hash)
            .or_insert_with(|| Bytes::from(data.to_vec()));
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Option<Bytes>, StoreError> {
        self.reads.set(self.reads.get() + 1);
        match self.chunks.borrow().get(hash) {
            Some(data) if data.len() as u64 > self.max_chunk_size => {
                Err(StoreError::ObjectTooLarge {
                    size: data.len() as u64,
                    limit: self.max_chunk_size,
                })
            }
            found => Ok(found.cloned()),
        }
    }

    fn max_chunk_size(&self) -> u64 {
        self.max_chunk_size
    }
}

// ---------------------------------------------------------------------------
// Deterministic pseudo-randomness (splitmix64) — bulk detail derived from
// seeds so proptest only has to generate (and shrink) small values.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform-ish in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// `len` deterministic pseudo-random bytes.
pub fn fill_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let chunk = rng.next_u64().to_be_bytes();
        let take = chunk.len().min(len - out.len());
        out.extend_from_slice(&chunk[..take]);
    }
    out
}

pub type Map = BTreeMap<Vec<u8>, Vec<u8>>;

/// `n` deterministic bulk entries (distinct keys, ~40–100-byte values)
/// that vary with `seed`. Cheap to generate and to shrink (only `n` and
/// `seed` are proptest-generated), used to push map sizes into the
/// thousands.
pub fn bulk_entries(n: usize, seed: u64) -> Map {
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
