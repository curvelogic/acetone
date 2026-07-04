//! Graph semantics and repository plumbing for acetone (spec §2–§4, §6).
//!
//! Phase 1 delivers the repository plumbing (spec §3.5 and §4):
//!
//! - [`repo::Repository`] — init/open an acetone repository (a bare git
//!   repository acetone owns), workspaces as manifest refs under
//!   `refs/acetone/workspaces/`, branches as ordinary git refs, commits
//!   as real git commits carrying the manifest and anchoring the
//!   complete chunk set of the version;
//! - [`repo::Transaction`] — the single-writer transaction (spec §4):
//!   staged raw-map mutations, atomic workspace advance by
//!   compare-and-swap, commit creation;
//! - [`repo::Snapshot`] — readers pinned to immutable manifests (MVCC;
//!   snapshot isolation by construction);
//! - [`lock::WriteLock`] — the single-writer lock file.
//!
//! Graph *semantics* — constraint enforcement, schema validation, index
//! maintenance, merge orchestration and the conflicts-as-data model —
//! arrive in later beads; Phase 1 mutations are deliberately raw
//! plumbing (ADR-0010).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod lock;
pub mod repo;

pub use error::GraphError;
pub use lock::WriteLock;
pub use repo::{
    DEFAULT_BRANCH, DEFAULT_WORKSPACE, InitOptions, LogEntry, Repository, Snapshot, Transaction,
};
