//! The storage traits: [`ChunkStore`], [`RefStore`] and [`CommitStore`].
//!
//! These traits are the seam between acetone and its storage substrate
//! (spec §3.1, ADR-0002): the prolly layer and everything above it program
//! against them and never against git. The contracts below are normative —
//! an implementation that violates them breaks the layers built on top.

use bytes::Bytes;

use crate::error::StoreError;
use crate::hash::Hash;

/// Content-addressed chunk storage.
///
/// # Contract
///
/// - **`put` is content-addressed and idempotent**: the returned [`Hash`]
///   is a pure function of the chunk bytes, so putting the same bytes twice
///   returns the same hash and stores one object. Callers may rely on this
///   for deduplication and for history independence (identical contents ⇒
///   identical addresses, however they were produced).
/// - **`get` distinguishes absence from damage**: `Ok(None)` means no
///   object with that address exists in the store; `Err(_)` means an object
///   is (or should be) present but cannot be returned — wrong kind, over
///   the size cap, or undecodable. Callers MUST NOT treat `Err` as absence.
/// - **Size cap**: every store has a maximum object size
///   ([`ChunkStore::max_chunk_size`]). `put` rejects larger chunks;
///   `get` checks the stored object's announced size *before* materialising
///   it and rejects anything over the cap, so a hostile object cannot cause
///   unbounded allocation.
/// - **Durability**: a chunk put here is durable in the object database but
///   not yet *reachable*; garbage collection may reclaim chunks that no
///   commit references (see [`CommitStore`]). Layers that need chunks to
///   survive `git gc` must anchor them to a commit.
pub trait ChunkStore {
    /// Store `data` as one chunk, returning its content address.
    ///
    /// Idempotent: identical bytes yield an identical hash. Errors with
    /// [`StoreError::ObjectTooLarge`] if `data` exceeds
    /// [`max_chunk_size`](ChunkStore::max_chunk_size).
    fn put(&self, data: &[u8]) -> Result<Hash, StoreError>;

    /// Store a batch of chunks, returning their addresses in order.
    ///
    /// Semantically identical to calling [`put`](ChunkStore::put) per chunk
    /// (and the default implementation does exactly that). The batch form
    /// exists so implementations can write a whole batch as a single git
    /// packfile (pack-on-write, bead acetone-63m.10) instead of loose
    /// objects; callers with many chunks SHOULD prefer it. On error,
    /// chunks earlier in the batch may or may not have been written —
    /// harmless, since unreferenced chunks are simply garbage.
    fn put_batch(&self, chunks: &[&[u8]]) -> Result<Vec<Hash>, StoreError> {
        chunks.iter().map(|chunk| self.put(chunk)).collect()
    }

    /// Fetch the chunk addressed by `hash`.
    ///
    /// Returns `Ok(None)` if no such object exists; `Err(_)` if an object
    /// exists but is not a returnable chunk (wrong kind, over the size cap)
    /// or the store itself failed. Never panics on corrupt data.
    fn get(&self, hash: &Hash) -> Result<Option<Bytes>, StoreError>;

    /// The store's hard cap on object size in bytes, enforced by both
    /// [`put`](ChunkStore::put) and [`get`](ChunkStore::get).
    fn max_chunk_size(&self) -> u64;
}

/// Named, atomically-updatable pointers into the store (git refs).
///
/// # Contract
///
/// - **Names are untrusted input**: every operation validates the name
///   against git ref-format rules first and rejects anything outside the
///   `refs/` namespace with [`StoreError::InvalidRefName`]. Implementations
///   MUST NOT derive filesystem paths from unvalidated names.
/// - **Writes are compare-and-swap**: `write_ref` succeeds only if the
///   ref's current value equals `expected` (`None` = the ref must not
///   exist) at the moment of the update, atomically. A lost race is
///   [`StoreError::CasFailed`] — read again and retry; there is no
///   unconditional overwrite in this API.
/// - **Reads distinguish absence from damage**: `Ok(None)` for a ref that
///   does not exist; `Err(_)` for one that exists but is unusable (e.g.
///   symbolic where a direct ref is required, undecodable ref storage).
pub trait RefStore {
    /// Read the current value of `name`, or `None` if the ref does not
    /// exist.
    fn read_ref(&self, name: &str) -> Result<Option<Hash>, StoreError>;

    /// Atomically set `name` to `new` if its current value is `expected`.
    ///
    /// `expected = None` means "create: the ref must not exist yet";
    /// `expected = Some(h)` means "update: the ref must currently be `h`".
    /// Fails with [`StoreError::CasFailed`] when the precondition does not
    /// hold at commit time.
    fn write_ref(&self, name: &str, expected: Option<&Hash>, new: &Hash) -> Result<(), StoreError>;
}

/// Author or committer identity for a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Human-readable name.
    pub name: String,
    /// Email address (not verified; git-native semantics).
    pub email: String,
}

impl Default for Signature {
    /// A neutral tool identity for commits created without explicit
    /// authorship. `.invalid` is the reserved TLD for placeholder
    /// addresses.
    fn default() -> Self {
        Signature {
            name: "acetone".into(),
            email: "acetone@acetone.invalid".into(),
        }
    }
}

/// Everything needed to create one acetone commit (spec §3.5).
#[derive(Debug, Clone)]
pub struct NewCommit<'a> {
    /// The manifest bytes; stored as the `manifest` blob in the commit
    /// tree. Opaque to this crate — the model layer defines the format.
    pub manifest: &'a [u8],
    /// A small human-readable summary, stored as `README.md` in the commit
    /// tree so hosting UIs show something meaningful.
    pub summary: &'a str,
    /// The commit message proper (subject and optional body), *without*
    /// trailers — those are supplied separately and appended as the final
    /// trailer paragraph.
    pub message: &'a str,
    /// Structured metadata trailers, e.g. `("Acetone-Source", "…")`,
    /// `("Acetone-Extractor", "…")`, `("Acetone-Source-Hash", "…")`.
    /// Tokens must match `[A-Za-z0-9][A-Za-z0-9-]*`; values must be single
    /// line, free of control characters.
    pub trailers: &'a [(String, String)],
    /// Parent commits, in order. Empty for a root commit.
    pub parents: &'a [Hash],
    /// Author and committer identity (git-native; both set to this value).
    pub author: Signature,
}

/// A commit read back from the store.
///
/// The commit's tree may contain entries beyond `manifest` and `README.md`
/// (future versions anchor chunk-reachability data there); readers ignore
/// what they do not understand.
#[derive(Debug, Clone)]
pub struct Commit {
    /// The commit's own address.
    pub id: Hash,
    /// The manifest bytes from the commit tree.
    pub manifest: Bytes,
    /// The full commit message as stored — subject, body *and* trailer
    /// paragraph. Lossily decoded to UTF-8 (commit messages are
    /// informational; hostile bytes must not make the commit unreadable).
    pub message: String,
    /// Trailers parsed from the message's final paragraph, in order.
    pub trailers: Vec<(String, String)>,
    /// Parent commit addresses, in order.
    pub parents: Vec<Hash>,
}

/// Version snapshots: real git commits whose tree carries the manifest
/// (spec §3.5).
///
/// # Contract
///
/// - **A commit anchors its tree**: the manifest and summary blobs of a
///   created commit are reachable from the commit and therefore survive
///   `git gc` for as long as some ref reaches the commit. Creating a commit
///   does *not* touch any ref — pair it with
///   [`RefStore::write_ref`] to make it reachable.
/// - **Reads distinguish absence from damage**: `Ok(None)` when no object
///   with that address exists; `Err(_)` when the object exists but is not a
///   well-formed acetone commit (not a commit, tree missing the `manifest`
///   entry, oversized or truncated objects). Never panics on corrupt data.
pub trait CommitStore {
    /// Write `commit` as a git commit object and return its address.
    ///
    /// The commit's tree contains the manifest blob (`manifest`) and the
    /// human-readable summary (`README.md`); trailers are appended to the
    /// message as the final paragraph in git trailer format.
    fn create_commit(&self, commit: &NewCommit<'_>) -> Result<Hash, StoreError>;

    /// Read a commit and its manifest bytes back.
    ///
    /// Returns `Ok(None)` if no object with this address exists. All size
    /// caps apply to the commit, its tree and the manifest blob.
    fn read_commit(&self, id: &Hash) -> Result<Option<Commit>, StoreError>;
}
