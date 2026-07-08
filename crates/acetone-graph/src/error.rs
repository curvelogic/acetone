//! Error type for the repository/graph layer.

use acetone_model::graph_keys::GraphKeyError;
use acetone_model::manifest::ManifestDecodeError;
use acetone_model::records::{RecordDecodeError, RecordEncodeError};
use acetone_model::schema::SchemaError;
use acetone_prolly::ProllyError;
use acetone_store::StoreError;
use std::path::PathBuf;
use thiserror::Error;

/// Errors from repository operations.
#[derive(Debug, Error)]
pub enum GraphError {
    /// The chunk/ref/commit store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A prolly-tree operation failed.
    #[error(transparent)]
    Prolly(#[from] ProllyError),
    /// The workspace or commit manifest did not decode.
    #[error(transparent)]
    Manifest(#[from] ManifestDecodeError),
    /// A graph key failed to encode or decode.
    #[error(transparent)]
    GraphKey(#[from] GraphKeyError),
    /// A record failed to encode.
    #[error(transparent)]
    RecordEncode(#[from] RecordEncodeError),
    /// A record failed to decode.
    #[error(transparent)]
    RecordDecode(#[from] RecordDecodeError),
    /// A schema entry failed to decode or validate.
    #[error(transparent)]
    Schema(#[from] SchemaError),
    /// Another writer holds the repository's single-writer lock
    /// (spec §4).
    #[error(
        "repository is locked by another writer ({holder}); if that process is dead, \
         remove {path} manually"
    )]
    Locked {
        /// Contents of the lock file (pid and timestamp of the holder).
        holder: String,
        /// The lock file's path, for the manual-recovery instruction.
        path: PathBuf,
    },
    /// The workspace ref moved underneath this transaction (another
    /// writer advanced it between load and save).
    #[error("workspace {name:?} changed concurrently; reload and retry")]
    WorkspaceConflict {
        /// Workspace name.
        name: String,
    },
    /// A branch ref moved underneath a commit (concurrent advance).
    #[error("branch {name:?} changed concurrently; reload and retry")]
    BranchConflict {
        /// Branch name.
        name: String,
    },
    /// The named workspace does not exist (repository not initialised
    /// with acetone, or wrong directory).
    #[error("no acetone workspace {name:?} in this repository — run acetone init?")]
    NoWorkspace {
        /// Workspace name.
        name: String,
    },
    /// A refspec did not resolve to a branch, ref or commit.
    #[error("cannot resolve {refspec:?} to a branch, ref or commit")]
    UnresolvedRefspec {
        /// The refspec as given.
        refspec: String,
    },
    /// A ref resolved but the object it names is not a readable commit.
    #[error("ref {name:?} does not point at an acetone commit")]
    NotACommit {
        /// The ref name.
        name: String,
    },
    /// Committing requires the checked-out ref to be a branch.
    #[error("cannot commit: the checked-out ref is not a branch")]
    NoCurrentBranch,
    /// The workspace has uncommitted changes that the operation would
    /// discard.
    #[error("workspace has uncommitted changes; commit them first")]
    DirtyWorkspace,
    /// The workspace is mid-merge (`conflicts` map present); this
    /// operation is not available until the merge completes (Phase 4
    /// delivers merge completion).
    #[error("workspace is in a merge; resolve and complete it first")]
    MergeInProgress,
    /// A branch that was expected to exist does not.
    #[error("no such branch {name:?}")]
    NoSuchBranch {
        /// Branch name.
        name: String,
    },
    /// A branch that was expected not to exist already does.
    #[error("branch {name:?} already exists")]
    BranchExists {
        /// Branch name.
        name: String,
    },
    /// A node the operation targets does not exist.
    #[error("no node {label:?} with key {key}")]
    NoSuchNode {
        /// Primary label.
        label: String,
        /// Rendered key.
        key: String,
    },
    /// A rekey target key is already taken by another node.
    #[error("cannot rekey to {label:?} {key}: a node with that key already exists")]
    RekeyConflict {
        /// Primary label.
        label: String,
        /// Rendered key.
        key: String,
    },
    /// A merge input carries state the merge does not yet handle.
    #[error("merge is not yet supported for {feature}")]
    MergeUnsupported {
        /// The unsupported aspect (e.g. secondary indexes).
        feature: &'static str,
    },
    /// Two versions share no common ancestor, so there is no base for a
    /// three-way merge (unrelated histories).
    #[error("cannot merge {theirs} into {ours}: no common ancestor (unrelated histories)")]
    NoMergeBase {
        /// The current branch's head, in hex.
        ours: String,
        /// The version being merged in, in hex.
        theirs: String,
    },
    /// The persisted `conflicts` map did not decode (spec §6).
    #[error("corrupt conflicts map: {reason}")]
    CorruptConflicts {
        /// What was malformed.
        reason: &'static str,
    },
    /// An operation needs a merge in progress but none is (or vice versa).
    #[error("{0}")]
    MergeState(&'static str),
    /// An import extractor failed, or a source record could not be mapped to
    /// a canonical node/edge record (spec §7, ADR-0021).
    #[error(transparent)]
    Import(#[from] crate::import::ImportError),
    /// Creating or inspecting the lock file failed for filesystem
    /// reasons other than the lock being held.
    #[error("lock file I/O at {path}: {source}")]
    LockIo {
        /// The lock file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}
