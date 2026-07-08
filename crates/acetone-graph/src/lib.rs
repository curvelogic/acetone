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
//! - [`lock::WriteLock`] — the single-writer lock file;
//! - [`fsck::check`] — the integrity verifier (spec §7): chunk
//!   reachability and manifest integrity over every workspace and every
//!   reachable commit, reporting missing and corrupt chunks distinctly.
//!
//! Graph *semantics* — constraint enforcement, schema validation, index
//! maintenance, merge orchestration and the conflicts-as-data model —
//! arrive in later beads; Phase 1 mutations are deliberately raw
//! plumbing (ADR-0010).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod conflicts;
pub mod diff;
pub mod error;
pub mod fsck;
pub mod import;
pub mod lock;
pub mod merge;
pub mod repo;

pub use error::GraphError;
pub use fsck::{Finding, FindingKind, FsckReport, MapId, Origin, Severity, check as fsck};
pub use import::{
    EndpointRef, ImportError, ImportOptions, ImportOutcome, ImportRecord, Provenance,
    SourceExtractor, run as import,
};
pub use lock::WriteLock;
pub use repo::{
    DEFAULT_BRANCH, DEFAULT_WORKSPACE, InitOptions, LogEntry, Repository, Snapshot, Transaction,
};
