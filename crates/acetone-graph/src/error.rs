//! Error type for the repository/graph layer.

use acetone_model::graph_keys::GraphKeyError;
use acetone_model::manifest::ManifestDecodeError;
use acetone_model::records::{RecordDecodeError, RecordEncodeError};
use acetone_model::schema::SchemaError;
use acetone_model::values::ValueEncodeError;
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
    /// A property value failed to canonically encode (cell-wise merge equality).
    #[error(transparent)]
    ValueEncode(#[from] ValueEncodeError),
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
    /// A schema change would alter a label's key tuple while nodes bearing that
    /// label already exist. Node identity is `(primary label, key tuple)`
    /// (Invariant #3), so changing the key orphans every existing node's key
    /// from the schema — an unsupported mutation. Redeclare the key only before
    /// adding data, or evolve it with `migrate`.
    #[error(
        "cannot change the key of label {label:?}: nodes already exist under its current key \
         (node identity is immutable — redeclare before adding data, or use migrate)"
    )]
    LabelKeyChanged {
        /// The label whose key tuple the change would alter.
        label: String,
    },
    /// A write would leave an edge without one of its endpoint nodes present
    /// (referential integrity — Invariant #3 / ADR-0028). The transaction is
    /// rejected before it can commit a structurally invalid graph.
    #[error(
        "operation would leave a dangling {rtype} relationship: its {role} endpoint node {endpoint} does not exist"
    )]
    DanglingEdge {
        /// The relationship type of the offending edge.
        rtype: String,
        /// Which endpoint is missing ("source" or "target").
        role: &'static str,
        /// The rendered missing endpoint (label and key).
        endpoint: String,
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
    /// An `edges_rev` entry has no matching `edges_fwd` record — the reverse map
    /// is derived from the forward map by construction (Invariant #5), so this
    /// can only mean a corrupt reverse map (fsck's edge-symmetry check catches
    /// it). Surfaced rather than silently dropped when reading in-edges.
    #[error("corrupt reverse edge map: {edge} has no forward record")]
    InconsistentReverseEdge {
        /// The rendered offending edge.
        edge: String,
    },
    /// An operation needs a merge in progress but none is (or vice versa).
    #[error("{0}")]
    MergeState(&'static str),
    /// A history rewrite (`acetone migrate`) hit an internal inconsistency
    /// (a cycle in the commit graph, or a reachable commit that vanished).
    #[error("migrate: {0}")]
    Migrate(String),
    /// An import extractor failed, or a source record could not be mapped to
    /// a canonical node/edge record (spec §7, ADR-0021).
    #[error(transparent)]
    Import(#[from] crate::import::ImportError),
    /// `gc` was asked to run while linked worktrees exist. Consolidation's
    /// reachability walk cannot see another worktree's private refs
    /// (`refs/worktree/*`, ADR-0014), so pruning could destroy their
    /// uncommitted or mid-merge state; it refuses until made worktree-aware.
    #[error(
        "gc is not safe while linked worktrees exist (it could prune their \
         uncommitted work); run it with a single worktree"
    )]
    GcWithLinkedWorktrees,
    /// A linked worktree's git-dir basename (its worktree id) is not usable
    /// as a ref-name component, so its durability anchor (ADR-0044) cannot be
    /// written. Failing the save loudly is deliberate: silently skipping the
    /// anchor would revert this worktree to the pre-fix, foreign-gc-vulnerable
    /// state without any signal.
    #[error(
        "cannot anchor linked worktree {id:?} for gc-durability: its id is not \
         a usable ref name"
    )]
    WorktreeAnchorUnnameable {
        /// The offending worktree id (git-dir basename), lossily rendered.
        id: String,
    },
    /// Creating or inspecting the lock file failed for filesystem
    /// reasons other than the lock being held.
    #[error("lock file I/O at {path}: {source}")]
    LockIo {
        /// The lock file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A co-tenant `init` was asked to add a graph whose name is not a valid
    /// single ref-path component (empty, containing `/` or `..`, or otherwise
    /// rejected by git's ref-format rules). The graph name namespaces the
    /// graph's refs (ADR-0050), so it must be a well-formed ref component.
    #[error("invalid graph name {name:?}: {reason}")]
    InvalidGraphName {
        /// The offending graph name.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A co-tenant `init` was asked to add a graph that this repository already
    /// hosts (its marker ref already exists).
    #[error("graph {name:?} already exists in this repository")]
    GraphExists {
        /// The graph name.
        name: String,
    },
    /// A co-tenant `init` targeted a repository that already contains a
    /// standalone acetone workspace of its own. Co-tenant init starts a fresh
    /// graph and shares the per-worktree workspace ref, so it cannot be layered
    /// onto an existing acetone repository (ADR-0050).
    #[error(
        "repository already contains a standalone acetone workspace; cannot add \
         a co-tenant graph to it"
    )]
    ExistingAcetoneWorkspace,
    /// `open` found more than one co-tenant graph marker in the repository.
    /// Selecting among several graphs is not supported in 0.3 (single graph
    /// per repository); the layout is parameterised for it but the ergonomics
    /// are deferred (ADR-0050).
    #[error(
        "repository hosts multiple acetone graphs ({names:?}); selecting among \
         them is not supported yet"
    )]
    MultipleGraphs {
        /// The graph names found.
        names: Vec<String>,
    },
}
