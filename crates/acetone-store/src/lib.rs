//! Content-addressed chunk storage for acetone (spec §3.1, ADR-0002).
//!
//! This crate is the bottom of the acetone stack: the prolly layer and
//! everything above it build on the traits defined here and never touch git
//! directly. Three traits carry the whole contract:
//!
//! - [`ChunkStore`] — content-addressed blobs. `put` is **content-addressed
//!   and idempotent** (the hash is a pure function of the bytes); `get`
//!   returns `Ok(None)` for an *absent* object and `Err` for one that is
//!   present but invalid or over the size cap — callers must never confuse
//!   the two.
//! - [`RefStore`] — named pointers with **atomic compare-and-swap** update
//!   semantics and mandatory git ref-format validation of names.
//! - [`CommitStore`] — version snapshots as real git commits carrying the
//!   manifest, a human-readable summary and structured trailers
//!   (spec §3.5).
//!
//! The reference implementation, [`GitStore`], stores chunks as git blobs
//! in the object database of a real git repository: a chunk's address *is*
//! its git object ID, refs are git refs, commits are git commits, so the
//! entire git ecosystem (clone, push, hosting, signing, `git log`) applies
//! to acetone data unchanged.
//!
//! # Hostile input
//!
//! Everything read from a repository — objects, refs, commit metadata — is
//! treated as untrusted: repositories are opened with gix's isolation
//! options at reduced trust (see [`GitStore`] for exactly what that
//! disables), every decode path returns [`StoreError`] rather than
//! panicking, ref names are validated before use, and object sizes are
//! checked against a hard cap *before* the object is materialised.
//!
//! # Durability model
//!
//! A chunk written by [`ChunkStore::put`] is stored but **unreachable**:
//! git prunes unreachable objects on `git gc` and — just as important —
//! **does not transfer them** on `git clone`/`push`/`fetch`, and git
//! cannot parse manifests, so a chunk address stored inside manifest or
//! chunk bytes keeps nothing alive. [`CommitStore::create_commit`] is the
//! anchoring mechanism: it makes the manifest, the summary and every chunk
//! listed in [`NewCommit::anchors`] reachable from the commit — pass the
//! complete chunk set of the version, and pair the commit with
//! [`RefStore::write_ref`] so a ref reaches the commit.
//!
//! # Consolidation, and why `git gc` is safe-but-lossy (ADR-0011)
//!
//! Per-commit writes land as loose objects, which git's own delta heuristics
//! compress poorly: content-addressed chunks have no stable path and no size
//! correlation, so a rewritten chunk is never paired with its predecessor and
//! history retains many times the changed bytes. [`GitStore::consolidate`] is
//! acetone's own garbage-collecting repack: it rewrites the reachable object
//! set into one self-contained pack whose entries are deltas against the
//! predecessors recorded by [`GitStore::record_base_hints`] (which the prolly
//! layer discovers at write time), recovering roughly a 7× retention win over
//! `git gc`. It is **representation-only** — every object's bytes and address
//! are preserved exactly — so no chunk address or prolly root hash ever
//! changes.
//!
//! Running stock **`git gc`/`git repack` on an acetone repository is
//! safe-but-lossy**: it corrupts nothing and every object still reads back,
//! but it discards acetone's hand-chosen deltas and lands back near the
//! poorly-compressed baseline. Re-running [`GitStore::consolidate`] restores
//! the ratio.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod consolidate;
mod error;
mod git;
mod hash;
mod pack;
mod store;

pub use bytes::Bytes;

pub use consolidate::{ConsolidateOptions, ConsolidateStats};
pub use error::StoreError;
pub use git::{
    DEFAULT_MAX_CHUNK_SIZE, GitStore, GitStoreOptions, MAX_SYMREF_DEPTH, MAX_TAG_PEEL_DEPTH,
    ObjectFormat, validate_trailer,
};
pub use hash::Hash;
pub use store::{
    ChunkStore, Commit, CommitStore, Identity, NewCommit, RefStore, RefSwing, RewriteCommit,
    Signature, TagObject,
};
