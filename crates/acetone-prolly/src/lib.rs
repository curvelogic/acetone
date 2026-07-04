//! Prolly trees for acetone (spec §3.2).
//!
//! Every persistent acetone map is a prolly tree over the
//! [`ChunkStore`](acetone_store::ChunkStore) trait: an ordered map from
//! byte-string keys to byte-string values, stored as content-addressed
//! chunks with content-defined boundaries (~4 KiB mean). This crate knows
//! nothing about acetone's key/value encodings — the model layer sits
//! above it (spec §8) — and nothing about git: it programs exclusively
//! against the store traits.
//!
//! # Operations (spec §3.2, all required)
//!
//! - [`bulk_load`] / [`empty`] — build a map, returning its [`Root`];
//! - [`get`] — point lookup, loading only one root→leaf path;
//! - [`scan`] / [`scan_rev`] — ordered range scans, forward and reverse;
//! - [`apply_batch`] — batched insert/delete producing a new root,
//!   reusing every untouched chunk;
//! - [`diff`] — ordered stream of `(key, before, after)` between two
//!   roots, skipping shared subtrees (O(changed keys));
//! - [`merge`] — three-way merge returning a merged root plus an ordered
//!   key-level conflict stream;
//! - [`reachable_chunks`] / [`collect_reachable_chunks`] — the complete
//!   transitive chunk set of a root, for commit anchoring
//!   (`acetone_store::NewCommit::anchors`).
//!
//! # Load-bearing invariants
//!
//! **History independence** (invariant 1, normative): identical map
//! contents yield identical root hashes regardless of operation order —
//! [`apply_batch`] is provably bit-identical to a fresh [`bulk_load`] of
//! the resulting contents. **Merge determinism** (invariant 4): [`merge`]
//! is a pure function of `(base, ours, theirs)`; conflicts are data, not
//! errors. Both are enforced by this crate's property suites.
//!
//! # Hostile input
//!
//! Chunks are untrusted (repositories arrive over the network). Every
//! decode returns [`ProllyError`] instead of panicking; allocation is
//! bounded by actual input, never by declared counts; every read is
//! validated against its position in the tree (level tag, parent boundary
//! claims, key ordering), so wrong-but-well-formed chunks yield
//! [`ProllyError::Corrupt`] rather than wrong answers.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod chunker;
mod diff;
mod error;
mod merge;
mod node;
mod scan;
mod tree;
mod walk;

pub use acetone_store::{Bytes, Hash};

pub use chunker::{ChunkParams, MAX_CHUNK_MAX_BYTES};
pub use diff::{Diff, DiffEntry, diff};
pub use error::ProllyError;
pub use merge::{Conflict, MergeOutcome, merge};
pub use scan::{Scan, scan, scan_rev};
pub use tree::{apply_batch, bulk_load, empty, get};
pub use walk::{collect_reachable_chunks, reachable_chunks};

/// Maximum tree height (levels). Fan-out keeps real trees in single
/// digits; the bound exists so descents over hostile roots terminate.
pub const MAX_HEIGHT: u32 = 64;

/// Maximum key length in bytes (the format's `u32` length frame).
pub const MAX_KEY_LEN: usize = u32::MAX as usize;

/// Maximum value length in bytes (the format's `u32` length frame). A
/// practical ceiling arrives much earlier: a single entry must fit within
/// the store's object-size cap.
pub const MAX_VALUE_LEN: usize = u32::MAX as usize;

/// Root of one map version: everything needed to read the map back.
///
/// The manifest layer persists these fields and reconstructs the root via
/// [`Root::new`], which validates untrusted input; a `Root` in existence
/// always has a height in `1..=MAX_HEIGHT` and valid parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    hash: Hash,
    height: u32,
    params: ChunkParams,
}

impl Root {
    /// Reconstruct a root from persisted (untrusted) fields. Errors —
    /// never panics — on a height outside `1..=MAX_HEIGHT`. Whether the
    /// hash actually resolves to a chunk is discovered on first use.
    pub fn new(hash: Hash, height: u32, params: ChunkParams) -> Result<Self, ProllyError> {
        if height == 0 || height > MAX_HEIGHT {
            return Err(ProllyError::InvalidRoot {
                reason: format!("height {height} outside 1..={MAX_HEIGHT}"),
            });
        }
        Ok(Root {
            hash,
            height,
            params,
        })
    }

    /// Content address of the root chunk.
    pub fn hash(&self) -> Hash {
        self.hash
    }

    /// Number of levels (1 = the root is a leaf).
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The chunking parameters the tree was built with.
    pub fn params(&self) -> ChunkParams {
        self.params
    }

    /// The level of the root node (`height - 1`), as the u8 the node
    /// format carries. In range by construction: height is validated
    /// against [`MAX_HEIGHT`] (≤ 64) wherever a `Root` is created.
    pub(crate) fn top_level(&self) -> u8 {
        u8::try_from(self.height - 1).expect("height validated against MAX_HEIGHT")
    }
}

/// One mutation in a batch. Within a batch, duplicate keys resolve to the
/// last op submitted; deleting an absent key is a no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOp {
    /// Insert or replace `key` with `value`.
    Put(Vec<u8>, Vec<u8>),
    /// Remove `key` if present.
    Delete(Vec<u8>),
}

impl BatchOp {
    /// The key this op targets.
    pub fn key(&self) -> &[u8] {
        match self {
            BatchOp::Put(k, _) => k,
            BatchOp::Delete(k) => k,
        }
    }
}
