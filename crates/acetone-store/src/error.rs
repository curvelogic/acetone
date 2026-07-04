//! Error taxonomy for the store layer.
//!
//! Every fallible operation returns [`StoreError`]; no code path in this
//! crate panics on untrusted data (hostile repositories, corrupt objects,
//! remote-supplied ref names). The variants distinguish the cases callers
//! genuinely branch on — absence is *not* an error (see the trait contracts
//! in [`crate::store`]): `Ok(None)` means "not there", `Err` means "there
//! but unusable" or "could not tell".

use crate::hash::Hash;

/// Errors from chunk, ref and commit operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// An object exceeds the store's size cap — either data handed to
    /// `put`, or a stored object whose header announces a size above the
    /// cap. On the read side this is checked *before* the object is
    /// materialised, so a hostile multi-GiB object cannot exhaust memory.
    #[error("object of {size} bytes exceeds the store's maximum of {limit} bytes")]
    ObjectTooLarge {
        /// The announced or supplied size.
        size: u64,
        /// The store's configured cap.
        limit: u64,
    },

    /// An object exists but has the wrong git type for the requested
    /// operation (e.g. `get` on a tree, `read_commit` on a blob).
    #[error("object {hash} is a {actual}, expected a {expected}")]
    WrongObjectKind {
        /// The address that was looked up.
        hash: Hash,
        /// What the operation required.
        expected: &'static str,
        /// What was actually found.
        actual: String,
    },

    /// A ref name failed git ref-format validation, or is outside the
    /// `refs/` namespace. Ref names arriving from remotes are untrusted
    /// strings; nothing is done with a name until it has passed this check,
    /// and filesystem paths are never derived from names by this crate.
    #[error("invalid ref name {name:?}: {reason}")]
    InvalidRefName {
        /// The offending name, verbatim.
        name: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A ref exists but is symbolic, where a direct (object) ref was
    /// required. Acetone refs are always direct; a symbolic ref here means
    /// the repository was manipulated by something else.
    #[error("ref {name:?} is symbolic, expected a direct ref")]
    SymbolicRef {
        /// The ref that was looked up.
        name: String,
    },

    /// A compare-and-swap ref update lost the race: the ref's current value
    /// did not match the expected value (or the ref already existed when
    /// `expected` was `None`). Re-read the ref and retry or report.
    #[error("compare-and-swap on ref {name:?} failed: current value did not match expectation")]
    CasFailed {
        /// The ref being updated.
        name: String,
    },

    /// Bytes or hex that do not form a valid hash for any supported object
    /// format.
    #[error("invalid hash: {reason}")]
    InvalidHash {
        /// Why the input was rejected.
        reason: String,
    },

    /// A commit trailer token or value that cannot be represented in the
    /// git trailer format (spec §3.5).
    #[error("invalid commit trailer {token:?}: {reason}")]
    InvalidTrailer {
        /// The trailer token as supplied.
        token: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A commit signature (author/committer) field that git cannot store
    /// faithfully.
    #[error("invalid signature: {reason}")]
    InvalidSignature {
        /// Why it was rejected.
        reason: String,
    },

    /// An object was found but its contents do not decode as the structure
    /// acetone expects — a truncated commit, a commit tree without a
    /// manifest entry, and so on. Distinct from absence (`Ok(None)`).
    #[error("corrupt {context}: {reason}")]
    Corrupt {
        /// What was being decoded.
        context: &'static str,
        /// Why decoding failed.
        reason: String,
    },

    /// Any error surfaced by the underlying git substrate (I/O, zlib,
    /// packfile decoding, lock contention…), tagged with what the store was
    /// doing at the time.
    #[error("git backend error while {context}: {source}")]
    Backend {
        /// What the store was doing.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl StoreError {
    /// Wrap a backend error with operation context.
    pub(crate) fn backend(
        context: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        StoreError::Backend {
            context,
            source: Box::new(source),
        }
    }

    pub(crate) fn corrupt(context: &'static str, reason: impl Into<String>) -> Self {
        StoreError::Corrupt {
            context,
            reason: reason.into(),
        }
    }
}
