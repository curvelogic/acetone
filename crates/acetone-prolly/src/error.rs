//! Error taxonomy for the prolly-tree layer.
//!
//! Every fallible operation returns [`ProllyError`]; no code path in this
//! crate panics on untrusted data. Chunks read from the store are hostile
//! input (spec §3.1: repositories arrive over the network): a chunk that
//! does not decode as the structure this crate expects — wrong level tag,
//! keys out of order, truncated frames, trailing bytes, a parent whose
//! declared child boundary the child does not honour — yields
//! [`ProllyError::Corrupt`], never a panic and never a wrong answer.

use acetone_store::{Hash, StoreError};

/// Errors from prolly-tree operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProllyError {
    /// The underlying chunk store failed (I/O, size caps, backend errors).
    #[error(transparent)]
    Store(#[from] StoreError),

    /// The tree references a chunk the store does not have. Distinct from
    /// [`StoreError`]: the store worked correctly and reported absence; the
    /// *tree* is dangling (a gc'd or untransferred chunk — see the
    /// anchoring contract on `acetone_store::NewCommit::anchors`).
    #[error("missing chunk {hash}: referenced by the tree but absent from the store")]
    MissingChunk {
        /// The dangling reference.
        hash: Hash,
    },

    /// A chunk was fetched but its contents are not a well-formed tree node
    /// consistent with its position in the tree.
    #[error("corrupt {context}: {reason}")]
    Corrupt {
        /// What was being decoded or checked.
        context: &'static str,
        /// Why it was rejected.
        reason: String,
    },

    /// Chunking parameters outside the format's valid range (see
    /// [`ChunkParams::new`](crate::ChunkParams::new)).
    #[error("invalid chunk parameters: {reason}")]
    InvalidParams {
        /// Why the parameters were rejected.
        reason: String,
    },

    /// A root descriptor outside the format's valid range (see
    /// [`Root::new`](crate::Root::new)).
    #[error("invalid root: {reason}")]
    InvalidRoot {
        /// Why the root was rejected.
        reason: String,
    },

    /// A key longer than the format's `u32` length frame can carry.
    /// Rejected on the write path — never silently truncated.
    #[error("key of {len} bytes exceeds the format maximum of {max} bytes")]
    KeyTooLong {
        /// The offending length.
        len: usize,
        /// The format maximum ([`crate::MAX_KEY_LEN`]).
        max: usize,
    },

    /// A value longer than the format's `u32` length frame can carry.
    /// Rejected on the write path — never silently truncated.
    #[error("value of {len} bytes exceeds the format maximum of {max} bytes")]
    ValueTooLong {
        /// The offending length.
        len: usize,
        /// The format maximum ([`crate::MAX_VALUE_LEN`]).
        max: usize,
    },

    /// More entries accumulated in one chunk than the format's `u32` entry
    /// count can carry (unreachable with valid [`crate::ChunkParams`], but
    /// guarded rather than assumed).
    #[error("entry count overflow while building a chunk")]
    TooManyEntries,

    /// The inputs to [`merge`](crate::merge) were built with different
    /// chunking parameters. Chunk parameters are format-defining and fixed
    /// per repository (spec §3.2); roots with different parameters cannot
    /// belong to the same map history.
    #[error("merge inputs disagree on chunk parameters")]
    ParamsMismatch,
}

impl ProllyError {
    pub(crate) fn corrupt(context: &'static str, reason: impl Into<String>) -> Self {
        ProllyError::Corrupt {
            context,
            reason: reason.into(),
        }
    }
}
