//! The opaque content address used throughout acetone.

use crate::error::StoreError;

/// The content address of an object in the chunk store.
///
/// For the git backend this wraps a git object ID, but callers MUST treat it
/// as opaque (spec §3.1): the repository's object format (SHA-1 default,
/// SHA-256 supported) determines its width, and nothing above the store may
/// assume one or the other. `Hash` is `Copy`, totally ordered and hashable,
/// so it can be embedded in keys, chunk payloads and maps directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(gix::ObjectId);

impl Hash {
    /// The raw hash bytes (20 for SHA-1, 32 for SHA-256).
    ///
    /// This is the canonical serialised form for embedding a chunk address
    /// inside another chunk; [`Hash::from_bytes`] is its inverse.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Reconstruct a hash from bytes previously produced by
    /// [`Hash::as_bytes`].
    ///
    /// Errors (never panics) if `bytes` is not a supported digest length.
    /// The bytes are untrusted input: a valid length says nothing about
    /// whether the object exists — pass the result to
    /// [`ChunkStore::get`](crate::ChunkStore::get) to find out.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StoreError> {
        gix::ObjectId::try_from(bytes)
            .map(Hash)
            .map_err(|e| StoreError::InvalidHash {
                reason: e.to_string(),
            })
    }

    /// Lower-case hex representation (same as `Display`).
    pub fn to_hex(&self) -> String {
        self.0.to_string()
    }

    /// Parse a hash from its hex representation.
    ///
    /// Errors (never panics) on anything that is not a full-width hex
    /// digest; hex strings are untrusted input.
    pub fn from_hex(hex: &str) -> Result<Self, StoreError> {
        gix::ObjectId::from_hex(hex.as_bytes())
            .map(Hash)
            .map_err(|e| StoreError::InvalidHash {
                reason: e.to_string(),
            })
    }

    /// The underlying git object ID — crate-internal so the git substrate
    /// never leaks into the public API.
    pub(crate) fn oid(&self) -> gix::ObjectId {
        self.0
    }

    pub(crate) fn from_oid(oid: gix::ObjectId) -> Self {
        Hash(oid)
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
