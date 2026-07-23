//! Repository plumbing: workspace refs, write transactions and commits
//! (spec §3.5, §4; ADR-0010).
//!
//! A [`Repository`] wraps the [`GitStore`] of one acetone repository —
//! a bare git repository acetone owns outright. Its moving parts:
//!
//! - the **workspace**: a manifest blob referenced from
//!   `refs/acetone/workspaces/<name>` (default `default`), giving
//!   Dolt-style WORKING state that survives process exit. The ref is
//!   advanced by compare-and-swap, so a workspace update is atomic and a
//!   lost race is a typed error, never a lost write. The namespace is
//!   local-only — it is never pushed (transferable state lives in
//!   `refs/heads`/`refs/tags`).
//! - the **single-writer lock** ([`crate::lock::WriteLock`], spec §4):
//!   all mutation happens inside a [`Transaction`], which holds the lock
//!   for its lifetime. Readers ([`Snapshot`]) never take it — they are
//!   pinned to an immutable manifest (MVCC by construction).
//! - **commits**: `Transaction::commit` turns the workspace manifest
//!   into a real git commit (manifest + summary in the tree, trailers in
//!   the message, parents from the current branch) and advances the
//!   branch ref, again by compare-and-swap. The commit **anchors the
//!   complete chunk set** of every map root in the manifest, so `git
//!   gc`/`clone`/`push` preserve and transfer whole versions.
//!
//! # Uncommitted workspaces and git gc (acetone-huo)
//!
//! The per-worktree workspace ref points at a **workspace tree**
//! `{manifest, chunks/}` — the commit-tree shape minus the README — whose
//! `chunks/` anchor tree references every chunk the manifest needs. Git's
//! reachability walk follows the tree and keeps those chunks, so even an
//! aggressive foreign `git gc --prune=now` preserves an uncommitted
//! workspace in full. This closes ADR-0010's "commit before external gc"
//! caveat. The anchor tree references existing blobs, so it costs no chunk
//! storage and unchanged shard trees dedupe across saves; it is a
//! local-only ref-plumbing change (no `format_version` bump). A workspace
//! last written before huo (a bare manifest blob) is read transparently
//! and rewritten as a tree on its next content change.

use crate::diff::{EdgeChange, GraphDiff, NodeChange};
use crate::error::GraphError;
use crate::lock::WriteLock;
use crate::merge::{ConflictMap, ManifestMerge, MergeOutcome, merge_manifests};
use crate::refns::GraphRefNamespace;
use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::{Manifest, MapRoot};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{IndexDef, SchemaEntry};
use acetone_prolly::{BatchOp, ChunkParams, Root, collect_reachable_chunks};
use acetone_store::{
    ChunkStore, CommitStore, ConsolidateOptions, ConsolidateStats, GitStore, GitStoreOptions, Hash,
    NewCommit, ObjectFormat, RefStore, Signature, StoreError,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::Path;

/// The per-worktree workspace ref (ADR-0014). Under git's `refs/worktree/*`
/// namespace, so git and gix resolve it per-worktree: each worktree has its
/// own working state, like its own index.
pub const WORKTREE_WORKSPACE_REF: &str = "refs/worktree/acetone/workspace";
/// The per-worktree `MERGE_HEAD`: names the `theirs` commit while a merge is
/// in progress (spec §6, acetone-14c.4). Present iff the workspace is
/// mid-merge; `commit` reads it as the second parent and clears it.
pub const WORKTREE_MERGE_HEAD_REF: &str = "refs/worktree/acetone/merge-head";
/// Legacy (pre-ADR-0014) shared workspace-ref namespace. Read as a
/// fallback so an existing repository keeps its workspace across the
/// upgrade; the first write migrates to [`WORKTREE_WORKSPACE_REF`].
pub const WORKSPACE_REF_PREFIX: &str = "refs/acetone/workspaces/";
/// Common-dir anchors that keep a *linked* worktree's uncommitted workspace
/// gc-durable (ADR-0044). A linked worktree's own workspace ref lives under
/// `refs/worktree/*`, which git does NOT enumerate as a gc root from another
/// worktree — so a foreign `git gc --prune=now` from the main worktree would
/// prune its saved-but-uncommitted chunks. Mirroring the workspace tree into
/// `refs/acetone/worktree-anchors/<worktree-id>` — an ordinary ref in the
/// common store, which git enumerates globally — closes that gap. Local-only,
/// like all `refs/acetone/*` (never transferred; operational-constraints).
pub const WORKTREE_ANCHOR_PREFIX: &str = "refs/acetone/worktree-anchors/";
/// Direct marker refs recording each co-tenant graph hosted in a repository
/// (ADR-0050). `refs/acetone/graphs/<name>` exists iff the repository hosts a
/// co-tenant graph `<name>`; `open` enumerates this prefix to detect the mode
/// and the graph name. A *direct* ref (not the graph's head symref, which
/// `list_refs` skips), so it is discoverable, and local-only like all
/// `refs/acetone/*`.
pub const GRAPHS_REF_PREFIX: &str = "refs/acetone/graphs/";
/// Namespace of branches (git-native). The standalone layout's branch
/// prefix; see [`GraphRefNamespace`](crate::refns::GraphRefNamespace).
pub const BRANCH_REF_PREFIX: &str = "refs/heads/";
/// Namespace of tags (git-native). The standalone layout's tag prefix.
pub const TAG_REF_PREFIX: &str = "refs/tags/";
/// The branch a fresh repository's checked-out ref points at.
pub const DEFAULT_BRANCH: &str = "main";
/// The default workspace name (one workspace per checkout in v0.1).
pub const DEFAULT_WORKSPACE: &str = "default";

/// Default chunking parameters: content-defined boundaries with a ~4 KiB
/// mean (spec §3.2), 1 KiB minimum and 64 KiB maximum.
pub fn default_chunk_params() -> ChunkParams {
    ChunkParams::new(1024, 12, 65536).expect("default parameters are valid")
}

/// Parameters for [`Repository::init`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InitOptions {
    /// Object format (hash function) of the new repository.
    pub object_format: ObjectFormat,
    /// Chunking parameters — fixed at init, recorded in every manifest
    /// (spec §3.2: changing them changes every hash).
    pub chunk_params: ChunkParams,
}

impl Default for InitOptions {
    fn default() -> Self {
        InitOptions {
            object_format: ObjectFormat::default(),
            chunk_params: default_chunk_params(),
        }
    }
}

/// Which side to take when bulk-resolving a merge's conflicts (spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveSide {
    /// The current branch's value (`--all-ours`).
    Ours,
    /// The merged-in version's value (`--all-theirs`).
    Theirs,
}

/// One commit as reported by [`Repository::log`].
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// The commit's address.
    pub id: Hash,
    /// The full commit message (subject, body and trailers).
    pub message: String,
    /// Parsed trailers.
    pub trailers: Vec<(String, String)>,
    /// Parent commit addresses.
    pub parents: Vec<Hash>,
}

/// An acetone repository: a bare git repository plus acetone's workspace
/// and lock conventions. See the module docs.
#[derive(Debug)]
pub struct Repository {
    store: GitStore,
    workspace: String,
    /// Where this graph's refs live (ADR-0049). Every branch/tag ref-path
    /// site resolves through it. Standalone for every repository today;
    /// `acetone-5w6` constructs a co-tenant layout here at `open`.
    namespace: GraphRefNamespace,
}

impl Repository {
    /// Create a new acetone repository at `path`: a bare git repository
    /// with an empty graph in the default workspace and the checked-out
    /// ref pointing at the (unborn) default branch. No commit is created
    /// — like Dolt, history starts with the first `commit`.
    pub fn init(path: &Path, options: InitOptions) -> Result<Repository, GraphError> {
        let mut store_options = GitStoreOptions::default();
        store_options.object_format = options.object_format;
        let store = GitStore::create_with(path, store_options)?;

        provision_empty_workspace(&store, options.chunk_params)?;
        let namespace = GraphRefNamespace::standalone();
        store.set_head(namespace.head_ref(), &namespace.branch_ref(DEFAULT_BRANCH))?;
        Ok(Repository {
            store,
            workspace: DEFAULT_WORKSPACE.to_owned(),
            namespace,
        })
    }

    /// Add a co-tenant acetone graph named `graph` to the **existing** git
    /// repository at `path` (ADR-0050). Unlike [`init`](Self::init), which
    /// creates a fresh bare repository that *is* the graph, this initialises an
    /// acetone graph *inside* a repository that already holds code: the graph's
    /// branches live under `refs/heads/acetone/<graph>/*`, its current-branch
    /// pointer at `refs/acetone/<graph>/HEAD`, and the code's `refs/heads/*` and
    /// git `HEAD` are left completely untouched.
    ///
    /// A direct marker ref `refs/acetone/graphs/<graph>` records the graph so
    /// [`open`](Self::open) can detect the mode. `options.object_format` is
    /// ignored — the existing repository's object format governs; only
    /// `options.chunk_params` is used, for the empty graph.
    ///
    /// Errors with [`GraphError::InvalidGraphName`] if `graph` is not a valid
    /// single ref-path component, [`GraphError::GraphExists`] if the repository
    /// already hosts a graph (0.3 is single-graph), and
    /// [`GraphError::ExistingAcetoneWorkspace`] if it already contains a
    /// standalone acetone workspace.
    pub fn init_co_tenant(
        path: &Path,
        graph: &str,
        options: InitOptions,
    ) -> Result<Repository, GraphError> {
        validate_graph_name(graph)?;
        let store = GitStore::open_discovering(path)?;

        // Preconditions, checked before ANY write so a rejected init leaves the
        // repository — and the user's code — completely untouched.
        // (1) The repository must not already host an acetone graph. Reporting
        //     the existing graph's name covers both a same-graph re-init and a
        //     second (unsupported) graph, and — crucially — refusing *before*
        //     writing the marker keeps a failed second init from leaving a
        //     stray marker that would make `open` see multiple graphs.
        if let Some((existing, _)) = store.list_refs(GRAPHS_REF_PREFIX)?.first() {
            let name = existing.strip_prefix(GRAPHS_REF_PREFIX).unwrap_or(existing);
            return Err(GraphError::GraphExists {
                name: name.to_owned(),
            });
        }
        // (2) Nor a standalone acetone workspace: co-tenant init starts a fresh
        //     graph and shares the per-worktree workspace ref, so it cannot be
        //     layered onto an existing acetone repository. Check both the
        //     per-worktree ref and the legacy pre-ADR-0014 shared workspace ref,
        //     so a legacy standalone repository is rejected too (otherwise a
        //     co-tenant graph could be layered onto it, and a later write would
        //     race two workspaces over the same refs).
        if store.read_ref(WORKTREE_WORKSPACE_REF)?.is_some()
            || store.read_ref(&workspace_ref(DEFAULT_WORKSPACE))?.is_some()
        {
            return Err(GraphError::ExistingAcetoneWorkspace);
        }

        // Write the marker FIRST (ADR-0050). The marker is what makes `open`
        // choose the safe co-tenant layout, so it must exist before the
        // workspace ref does: were the order reversed, a crash between
        // provisioning the workspace and writing the marker would leave a repo
        // that `open` reads as *standalone* — and a later write would commit
        // onto the user's `refs/heads/main`, destroying code. Marker-first, any
        // interrupted init instead opens as co-tenant and fails safely
        // (`NoWorkspace`/`NoCurrentBranch`), touching only `refs/acetone/*`.
        let filler = store.put(b"")?;
        let marker = format!("{GRAPHS_REF_PREFIX}{graph}");
        store.write_ref(&marker, None, &filler)?;

        provision_empty_workspace(&store, options.chunk_params)?;

        let namespace = GraphRefNamespace::co_tenant(graph);
        // The graph's own current-branch pointer; the code's git HEAD is not
        // touched.
        store.set_head(namespace.head_ref(), &namespace.branch_ref(DEFAULT_BRANCH))?;
        Ok(Repository {
            store,
            workspace: DEFAULT_WORKSPACE.to_owned(),
            namespace,
        })
    }

    /// Open an existing acetone repository (its default workspace),
    /// discovering it by walking up from `path` — so `path` may be any
    /// subdirectory of the repository, matching `git -C <path>` (ADR-0034).
    /// The discovered repository is opened with the same reduced-trust
    /// isolated options as an exact open; discovery changes only which
    /// directory is opened, never the trust posture.
    ///
    /// Errors with [`StoreError::NotARepository`] if no repository encloses
    /// `path` up to the discovery boundary (filesystem root or a
    /// `GIT_CEILING_DIRECTORIES` entry), and with [`GraphError::NoWorkspace`]
    /// if the discovered git repository was never initialised by acetone.
    ///
    /// The ref layout — standalone or co-tenant — is detected from the graph
    /// marker refs (ADR-0050): a repository with a `refs/acetone/graphs/<name>`
    /// marker opens in co-tenant mode for that graph; otherwise standalone.
    pub fn open(path: &Path) -> Result<Repository, GraphError> {
        let store = GitStore::open_discovering(path)?;
        let namespace = detect_namespace(&store)?;
        let repo = Repository {
            store,
            workspace: DEFAULT_WORKSPACE.to_owned(),
            namespace,
        };
        repo.ensure_workspace()?;
        // Fail fast: the workspace manifest must decode, so a damaged
        // repository is reported at open, not on first use.
        repo.workspace_manifest()?;
        Ok(repo)
    }

    /// Ensure the current worktree has a workspace. If it already has one
    /// (per-worktree or legacy ref), do nothing. Otherwise — a freshly
    /// `git worktree add`ed worktree that acetone has not seen — bootstrap
    /// its per-worktree workspace from the checked-out commit's committed
    /// manifest, so opening a new worktree "just works" (ADR-0014). The
    /// bootstrap takes the writer lock only in that first-time case; an
    /// already-provisioned worktree opens lock-free.
    ///
    /// The checked-out commit is resolved via [`GitStore::head_commit_id`], so a
    /// worktree at a **detached** `HEAD` bootstraps from its commit too, rather
    /// than failing every operation with a spurious "no workspace" error
    /// (acetone-cm9). Only a genuinely unborn `HEAD` (no commit at all) has
    /// nothing to bootstrap from.
    fn ensure_workspace(&self) -> Result<(), GraphError> {
        if self.workspace_ref_value()?.is_some() || self.legacy_ref_value()?.is_some() {
            return Ok(());
        }
        let _lock = WriteLock::acquire(self.store.git_dir())?;
        // Re-check under the lock: another process may have bootstrapped.
        if self.workspace_ref_value()?.is_some() {
            return Ok(());
        }
        match self.store.head_commit_id(self.namespace.head_ref())? {
            Some(commit) => {
                let manifest_hash = self.commit_manifest_hash(&commit)?;
                let tree = self.workspace_tree_for(&manifest_hash)?;
                self.cas_workspace(None, &tree)
            }
            // No workspace and an unborn HEAD (no commit): nothing to bootstrap
            // from.
            None => Err(GraphError::NoWorkspace {
                name: self.workspace.clone(),
            }),
        }
    }

    /// Build the workspace tree (`{manifest, chunks/}`, huo) for the manifest
    /// blob `manifest_hash`, anchoring its complete chunk set. Returns the
    /// tree hash the workspace ref should point at.
    pub(crate) fn workspace_tree_for(&self, manifest_hash: &Hash) -> Result<Hash, GraphError> {
        let manifest = read_manifest_chunk(&self.store, manifest_hash)?;
        let anchors = manifest_chunk_set(&self.store, &manifest)?;
        Ok(self
            .store
            .write_workspace_tree(&manifest.encode(), &anchors)?)
    }

    /// The underlying store (for layers building on the plumbing, e.g.
    /// fsck's reachability walk).
    pub fn store(&self) -> &GitStore {
        &self.store
    }

    /// The workspace's name.
    pub fn workspace_name(&self) -> &str {
        &self.workspace
    }

    /// The current value of the per-worktree workspace ref, if present.
    /// This is a workspace *tree* (huo) — or, for a workspace last written
    /// before huo, the manifest blob directly.
    fn workspace_ref_value(&self) -> Result<Option<Hash>, GraphError> {
        Ok(self.store.read_ref(WORKTREE_WORKSPACE_REF)?)
    }

    /// The current value of the legacy shared workspace ref (pre-ADR-0014
    /// migration fallback), if present.
    fn legacy_ref_value(&self) -> Result<Option<Hash>, GraphError> {
        Ok(self.store.read_ref(&workspace_ref(&self.workspace))?)
    }

    /// The effective workspace-ref target: the per-worktree ref, or — for a
    /// repository created before ADR-0014 — the legacy shared ref.
    fn workspace_ref_target(&self) -> Result<Option<Hash>, GraphError> {
        if let Some(hash) = self.workspace_ref_value()? {
            return Ok(Some(hash));
        }
        self.legacy_ref_value()
    }

    /// The manifest blob hash of the current workspace, resolving the ref
    /// target (a workspace tree, or a bare manifest blob for a pre-huo
    /// workspace) to the manifest blob.
    fn workspace_manifest_hash(&self) -> Result<Hash, GraphError> {
        let target = self
            .workspace_ref_target()?
            .ok_or_else(|| GraphError::NoWorkspace {
                name: self.workspace.clone(),
            })?;
        Ok(self.store.workspace_manifest_hash(&target)?)
    }

    /// The current workspace manifest (decoded and validated).
    pub fn workspace_manifest(&self) -> Result<Manifest, GraphError> {
        let hash = self.workspace_manifest_hash()?;
        read_manifest_chunk(&self.store, &hash)
    }

    /// A read-only view of the current workspace, pinned to its manifest
    /// (later workspace advances do not affect it — MVCC).
    pub fn workspace_snapshot(&self) -> Result<Snapshot<'_>, GraphError> {
        Ok(Snapshot {
            store: &self.store,
            manifest: self.workspace_manifest()?,
        })
    }

    /// Where this graph's refs live (ADR-0049): the layout every branch/tag
    /// ref-path resolves through. Standalone for every repository today.
    pub fn namespace(&self) -> &GraphRefNamespace {
        &self.namespace
    }

    /// The full ref name of the checked-out branch, if the checked-out
    /// ref is a branch.
    pub fn current_branch(&self) -> Result<Option<String>, GraphError> {
        Ok(self
            .store
            .read_head(self.namespace.head_ref())?
            .filter(|name| self.namespace.branch_name(name).is_some()))
    }

    /// The commit the checked-out branch points at (`None` while the
    /// branch is unborn).
    pub fn head_commit(&self) -> Result<Option<Hash>, GraphError> {
        match self.current_branch()? {
            None => Ok(None),
            Some(branch) => Ok(self.store.read_ref(&branch)?),
        }
    }

    /// Whether the workspace differs from the checked-out commit's committed
    /// state. True when `HEAD` is unborn and the workspace is no longer the
    /// empty graph it was initialised with — and, always, when the workspace
    /// manifest differs from the checked-out commit's. The checked-out commit is
    /// resolved via [`GitStore::head_commit_id`], so a **detached** worktree is
    /// compared against its checked-out commit (not spuriously against the empty
    /// graph, acetone-cm9), and a pristine detached bootstrap reads as clean.
    pub fn is_dirty(&self) -> Result<bool, GraphError> {
        let workspace = self.workspace_manifest_hash()?;
        match self.store.head_commit_id(self.namespace.head_ref())? {
            None => {
                // Unborn HEAD (no commit): dirty iff the workspace has left its
                // init state (an empty graph under the manifest's own
                // chunk parameters).
                let manifest = read_manifest_chunk(&self.store, &workspace)?;
                let empty = acetone_prolly::empty(&self.store, manifest.chunk_params)?;
                let blank = Manifest {
                    chunk_params: manifest.chunk_params,
                    schema: MapRoot::from_root(&empty),
                    nodes: MapRoot::from_root(&empty),
                    edges_fwd: MapRoot::from_root(&empty),
                    edges_rev: MapRoot::from_root(&empty),
                    indexes: Default::default(),
                    conflicts: None,
                };
                Ok(manifest != blank)
            }
            Some(head) => {
                let head_manifest_hash = self.commit_manifest_hash(&head)?;
                Ok(workspace != head_manifest_hash)
            }
        }
    }

    /// All branches, as `(short name, commit)` pairs in name order.
    pub fn branches(&self) -> Result<Vec<(String, Hash)>, GraphError> {
        Ok(self
            .store
            .list_refs(self.namespace.branch_prefix())?
            .into_iter()
            .map(|(name, hash)| {
                let short = self
                    .namespace
                    .branch_name(&name)
                    .unwrap_or(&name)
                    .to_owned();
                (short, hash)
            })
            .collect())
    }

    /// Create branch `name` at `refspec` (default: the current head
    /// commit). Does not check the branch out.
    pub fn create_branch(&self, name: &str, refspec: Option<&str>) -> Result<Hash, GraphError> {
        let target = match refspec {
            Some(spec) => self.resolve_commit(spec)?,
            None => self.head_commit()?.ok_or(GraphError::NoCurrentBranch)?,
        };
        let full = self.namespace.branch_ref(name);
        match self.store.write_ref(&full, None, &target) {
            Ok(()) => Ok(target),
            Err(StoreError::CasFailed { .. }) => Err(GraphError::BranchExists {
                name: name.to_owned(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Check out branch `name`: point the checked-out ref at it and reset
    /// the workspace to its committed manifest. Refuses to discard
    /// uncommitted changes ([`GraphError::DirtyWorkspace`]). Takes the
    /// single-writer lock (the workspace moves).
    pub fn checkout_branch(&self, name: &str) -> Result<(), GraphError> {
        let _lock = WriteLock::acquire(self.store.git_dir())?;
        if self.is_dirty()? {
            return Err(GraphError::DirtyWorkspace);
        }
        let full = self.namespace.branch_ref(name);
        let target = self
            .store
            .read_ref(&full)?
            .ok_or_else(|| GraphError::NoSuchBranch {
                name: name.to_owned(),
            })?;
        let manifest_hash = self.commit_manifest_hash(&target)?;
        // Reset the workspace to the branch's manifest. CAS the per-worktree
        // ref against its own current value (None when this is the first
        // write after an ADR-0014 migration — the CAS then creates it). Skip
        // only when the per-worktree ref already resolves to that manifest.
        let expected = self.workspace_ref_value()?;
        let current = match &expected {
            Some(hash) => Some(self.store.workspace_manifest_hash(hash)?),
            None => None,
        };
        if current != Some(manifest_hash) {
            let tree = self.workspace_tree_for(&manifest_hash)?;
            self.cas_workspace(expected.as_ref(), &tree)?;
        }
        self.store.set_head(self.namespace.head_ref(), &full)?;
        Ok(())
    }

    /// Rekey a node: change its identity from `old` to `new`. A key change
    /// is modelled as delete-plus-create (Invariant #3, spec §5.9): in one
    /// transaction the old node and its incident edges are removed and
    /// recreated under the new key with the same records, then committed —
    /// a single commit whose diff shows the transition. Errors if `old` is
    /// absent, if `new` already exists, or if `old == new`.
    pub fn rekey(&self, old: &NodeKey, new: &NodeKey, message: &str) -> Result<Hash, GraphError> {
        if old == new {
            return Err(GraphError::RekeyConflict {
                label: new.label().to_string(),
                key: render_node_key(new),
            });
        }
        let mut txn = self.begin_write()?;
        // Read the workspace this transaction locked.
        let snapshot = self.workspace_snapshot()?;
        let record = snapshot
            .get_node(old)?
            .ok_or_else(|| GraphError::NoSuchNode {
                label: old.label().to_string(),
                key: render_node_key(old),
            })?;
        if snapshot.get_node(new)?.is_some() {
            return Err(GraphError::RekeyConflict {
                label: new.label().to_string(),
                key: render_node_key(new),
            });
        }
        // Rewrite every incident edge onto the new endpoint (delete old,
        // put new) so no edge is left dangling.
        for (edge, edge_record) in snapshot.edges()? {
            let touches_src = edge.src() == old;
            let touches_dst = edge.dst() == old;
            if !touches_src && !touches_dst {
                continue;
            }
            let src = if touches_src { new } else { edge.src() };
            let dst = if touches_dst { new } else { edge.dst() };
            let rekeyed =
                EdgeKey::new(src.clone(), edge.rtype(), dst.clone(), edge.disc().clone())?;
            txn.delete_edge(&edge)?;
            txn.put_edge(&rekeyed, &edge_record)?;
        }
        // Delete the old node before creating the new one (ordering per
        // map): a same-label rekey shares neither key, so this is a plain
        // move, but the delete-before-put discipline keeps it robust.
        txn.delete_node(old)?;
        txn.put_node(new, &record)?;
        txn.commit(message, &[], None)
    }

    /// A read-only view pinned to `refspec` — a branch short name, a full
    /// ref name, or a commit hash in hex.
    pub fn snapshot(&self, refspec: &str) -> Result<Snapshot<'_>, GraphError> {
        let commit = self.resolve_commit(refspec)?;
        let manifest_hash = self.commit_manifest_hash(&commit)?;
        Ok(Snapshot {
            store: &self.store,
            manifest: read_manifest_chunk(&self.store, &manifest_hash)?,
        })
    }

    /// The graph-level difference from version `from` to version `to`
    /// (branch short names, full ref names or commit hashes) — the change
    /// stream behind `acetone diff` and `CALL acetone.diff()` (spec §7,
    /// acetone-14c.1). See [`crate::diff`].
    pub fn diff(&self, from: &str, to: &str) -> Result<GraphDiff, GraphError> {
        self.snapshot(from)?.diff(&self.snapshot(to)?)
    }

    /// Merge the version named by `theirs` into the current branch (spec §7,
    /// shaping Decision 4; acetone-14c.2).
    ///
    /// Preconditions, checked in this order: the checked-out ref is a branch
    /// ([`GraphError::NoCurrentBranch`]) — merging advances that branch, so a
    /// detached HEAD is unmergeable whatever the workspace state (acetone-060)
    /// — and the workspace has no uncommitted changes
    /// ([`GraphError::DirtyWorkspace`]).
    ///
    /// The four outcomes ([`MergeOutcome`]): **AlreadyUpToDate** when
    /// `theirs` is already an ancestor of our head; **FastForward** when our
    /// head is an ancestor of `theirs` (the branch simply advances, no merge
    /// commit); **Merged** when a genuine three-way merge over the merge base
    /// resolves cleanly (a two-parent merge commit `[ours, theirs]` is
    /// written and the branch advanced); **Conflicts** when it does not. On
    /// **cell** conflicts the workspace enters merge-in-progress — the
    /// conflicts are persisted and `MERGE_HEAD` names `theirs`, to be settled
    /// with `resolve` then `commit` (acetone-14c.4a). On **graph-level**
    /// violations, which have no resolution verb yet, the repository is left
    /// unchanged and the violations are only reported (acetone-14c.4c).
    ///
    /// The merge is a pure function of the three commit manifests (Invariant
    /// #4): `merge_manifests` depends only on their contents, and `edges_rev`
    /// is rebuilt from the merged forward map (Invariant #5). The merge
    /// commit's own hash is *not* reproducible (its author/committer
    /// timestamp is wall-clock), but its tree — the merged manifest — is.
    ///
    /// A map-clean merge is **graph-validated** before it becomes a `Merged`
    /// commit (acetone-14c.3): independently merging each map can still break
    /// referential integrity or a schema constraint — e.g. `ours` adds an
    /// edge to a node that `theirs` deletes, or both sides add nodes that
    /// collide on a UNIQUE property, with no key-level conflict in either map.
    /// Such a breach demotes the merge to `Conflicts` carrying
    /// [`crate::merge::GraphViolation`]s (data, not an error), so a `Merged`
    /// result is both map-clean and graph-valid.
    pub fn merge(&self, theirs: &str, message: &str) -> Result<MergeOutcome, GraphError> {
        let _lock = WriteLock::acquire(self.store.git_dir())?;
        // On-a-branch first: merge advances the current branch, so a detached
        // HEAD is unmergeable regardless of workspace state. Reporting
        // NoCurrentBranch before DirtyWorkspace gives the accurate cause when a
        // detached workspace also differs from its checked-out commit
        // (acetone-060).
        let branch = self.current_branch()?.ok_or(GraphError::NoCurrentBranch)?;
        if self.is_dirty()? {
            return Err(GraphError::DirtyWorkspace);
        }
        let theirs = self.resolve_commit(theirs)?;

        // An unborn branch has no head to merge against; adopt `theirs`
        // wholesale (a degenerate fast-forward that creates the branch ref).
        let Some(ours) = self.head_commit()? else {
            return self
                .fast_forward(&branch, None, &theirs)
                .map(MergeOutcome::FastForward);
        };

        // `theirs` already in our history (including equal): nothing to do.
        if ours == theirs || self.is_ancestor(&theirs, &ours)? {
            return Ok(MergeOutcome::AlreadyUpToDate);
        }
        // Our head is an ancestor of `theirs`: fast-forward, no merge commit.
        if self.is_ancestor(&ours, &theirs)? {
            return self
                .fast_forward(&branch, Some(&ours), &theirs)
                .map(MergeOutcome::FastForward);
        }

        // Genuine divergence: three-way merge over the merge base.
        let base = self
            .merge_base(&ours, &theirs)?
            .ok_or_else(|| GraphError::NoMergeBase {
                ours: ours.to_hex(),
                theirs: theirs.to_hex(),
            })?;
        let base_manifest = self.manifest_at_commit(&base)?;
        let ours_manifest = self.manifest_at_commit(&ours)?;
        let theirs_manifest = self.manifest_at_commit(&theirs)?;

        match merge_manifests(
            &self.store,
            &base_manifest,
            &ours_manifest,
            &theirs_manifest,
        )? {
            // A conflicted merge enters merge-in-progress state: the workspace
            // becomes the partial merge with a populated `conflicts` map, and
            // MERGE_HEAD names `theirs` for the later completion (spec §6,
            // acetone-14c.4). No commit is written.
            ManifestMerge::Conflicts {
                mut merged,
                conflicts,
            } => {
                // Both cell and graph-level conflicts now enter merge-in-progress
                // (acetone-mws). Cell conflicts resolve by picking a side
                // (`resolve --all-ours|--all-theirs`) or by writing the key;
                // graph violations (dangling edge / constraint) resolve by
                // ordinary writes that repair the graph, and the completion
                // commit re-validates before it lands. Either way `merge --abort`
                // is the escape hatch. The merged manifest for a graph-violation
                // merge is map-complete (the maps merged; validation flagged the
                // resulting graph), so the workspace shows that graph and the
                // conflicts map lists the violations to fix.
                let map = crate::conflicts::build_conflicts_map(
                    &self.store,
                    merged.chunk_params,
                    &conflicts,
                )?;
                merged.conflicts = Some(MapRoot::from_root(&map));
                let manifest_hash = self.store.put(&merged.encode())?;
                let tree = self.workspace_tree_for(&manifest_hash)?;
                let expected = self.workspace_ref_value()?;
                self.cas_workspace(expected.as_ref(), &tree)?;
                // Record the other side for `commit` to complete the merge.
                self.store
                    .write_ref(WORKTREE_MERGE_HEAD_REF, None, &theirs)
                    .or_else(|e| match e {
                        // A stale MERGE_HEAD from an abandoned merge: overwrite.
                        StoreError::CasFailed { .. } => {
                            self.store.delete_ref(WORKTREE_MERGE_HEAD_REF)?;
                            self.store.write_ref(WORKTREE_MERGE_HEAD_REF, None, &theirs)
                        }
                        other => Err(other),
                    })?;
                Ok(MergeOutcome::Conflicts(conflicts))
            }
            ManifestMerge::Clean(manifest) => {
                // Reset the workspace to the merged manifest, then write the
                // two-parent merge commit and advance the branch — the same
                // workspace-then-branch order as `Transaction::commit`.
                let manifest_hash = self.store.put(&manifest.encode())?;
                let tree = self.workspace_tree_for(&manifest_hash)?;
                let expected = self.workspace_ref_value()?;
                self.cas_workspace(expected.as_ref(), &tree)?;

                let commit = self.write_merge_commit(&manifest, &[ours, theirs], message)?;
                match self.store.write_ref(&branch, Some(&ours), &commit) {
                    Ok(()) => {
                        // A clean merge consumes no MERGE_HEAD, but clear any
                        // stale one (e.g. a prior completion whose delete failed,
                        // acetone-mws) so a later ordinary commit is not turned
                        // into a spurious merge commit.
                        if self.merge_head()?.is_some() {
                            self.store.delete_ref(WORKTREE_MERGE_HEAD_REF)?;
                        }
                        Ok(MergeOutcome::Merged(commit))
                    }
                    Err(StoreError::CasFailed { .. }) => Err(GraphError::BranchConflict {
                        name: branch.clone(),
                    }),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Abandon a merge in progress: discard the partial merge and its
    /// `conflicts` map, resetting the workspace to `ours` (the branch tip) and
    /// clearing `MERGE_HEAD` (`acetone merge --abort`, spec §6, acetone-mws).
    /// The escape hatch when you do not want to resolve the conflicts — and the
    /// only way to back out of a graph-violation merge. Errors if no merge is in
    /// progress.
    pub fn abort_merge(&self) -> Result<(), GraphError> {
        let _lock = WriteLock::acquire(self.store.git_dir())?;
        // A merge is abortable while MERGE_HEAD is set. Also accept a workspace
        // that still carries a `conflicts` map with no MERGE_HEAD: that is a
        // *half-aborted* state a prior abort could leave if its `delete_ref`
        // succeeded but a step failed — accepting either half means re-running
        // `merge --abort` always finishes the abort (idempotent recovery),
        // whichever step failed.
        let has_merge_head = self.merge_head()?.is_some();
        let has_conflicts = self.workspace_manifest()?.conflicts.is_some();
        if !has_merge_head && !has_conflicts {
            return Err(GraphError::MergeState("no merge in progress to abort"));
        }
        // Reset the workspace to the branch tip's manifest (its `conflicts` is
        // `None`, so the partial merge and its conflicts map are dropped).
        let ours = self.head_commit()?.ok_or(GraphError::MergeState(
            "merge in progress but the branch is unborn",
        ))?;
        let ours_manifest = self.manifest_at_commit(&ours)?;
        let manifest_hash = self.store.put(&ours_manifest.encode())?;
        let tree = self.workspace_tree_for(&manifest_hash)?;
        let expected = self.workspace_ref_value()?;
        self.cas_workspace(expected.as_ref(), &tree)?;
        // Clear MERGE_HEAD last — the definitive "merge over" signal. A failed
        // delete here leaves MERGE_HEAD set with the workspace already at the
        // branch tip; re-running `merge --abort` recovers it (and the commit
        // path's stale-MERGE_HEAD guard covers the post-completion analogue).
        if self.merge_head()?.is_some() {
            self.store.delete_ref(WORKTREE_MERGE_HEAD_REF)?;
        }
        Ok(())
    }

    /// Blame a node: the commits that changed its record, newest first (spec
    /// §5.2, `CALL acetone.blame`; acetone-14c.6). Walks the first-parent
    /// chain from HEAD and probes the node map at `key` in each commit; a
    /// commit is attributed the change when its record differs from the next
    /// (older) commit's — an introduction (older absent), a property change,
    /// or a deletion. The probe is `O(log n)` per commit.
    ///
    /// First-parent semantics: a change merged in through a two-parent merge
    /// commit is attributed to that merge commit (its record differs from its
    /// first parent's), the same convention `git blame --first-parent` uses.
    ///
    /// The key's arity is checked against the label's declared key tuple in
    /// the workspace schema — a mismatch is a typed
    /// [`GraphError::KeyArityMismatch`] (acetone-596). A label *not* declared
    /// in the current schema is probed schema-free and returns an **empty
    /// result** rather than erroring (raw-plumbing graphs, and labels dropped
    /// from the schema whose history is still blameable).
    pub fn blame(&self, key: &NodeKey) -> Result<Vec<Hash>, GraphError> {
        // Guard the key arity against the label's declared key tuple
        // (acetone-596): probing a composite-key label with a single value
        // (the CLI's single-column key plumbing) would otherwise silently
        // find nothing — a wrong answer, not an empty history. An
        // *undeclared* label stays schema-free: raw-plumbing graphs and
        // labels dropped from the current schema still blame by probing.
        let declared = self
            .workspace_snapshot()?
            .schema_entries()?
            .into_iter()
            .find_map(|entry| match entry {
                SchemaEntry::Label { name, def } if name == key.label() => Some(def),
                _ => None,
            });
        if let Some(def) = declared
            && def.key().len() != key.key().len()
        {
            let columns = def
                .key()
                .iter()
                .map(|name| acetone_model::display::format_label(name))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(GraphError::KeyArityMismatch {
                label: key.label().to_owned(),
                columns: format!("[{columns}]"),
                expected: def.key().len(),
                got: key.key().len(),
            });
        }

        // First-parent chain from HEAD, newest first (as `log` walks it).
        let mut chain = Vec::new();
        let mut seen = BTreeSet::new();
        let mut next = self.head_commit()?;
        while let Some(id) = next {
            if !seen.insert(id) {
                break; // defensive: valid git data has no cycles
            }
            let commit = self
                .store
                .read_commit(&id)?
                .ok_or_else(|| GraphError::NotACommit { name: id.to_hex() })?;
            chain.push(id);
            next = commit.parents.first().copied();
        }

        // Walk oldest → newest carrying the older commit's record forward, so
        // each commit is probed exactly once. A commit is credited when its
        // record differs from the older one — introduction (older absent), a
        // property change, or a deletion.
        let mut touching = Vec::new();
        let mut older = None;
        for commit in chain.iter().rev() {
            let current = self.record_at(commit, key)?;
            if current != older {
                touching.push(*commit);
            }
            older = current;
        }
        touching.reverse(); // newest first
        Ok(touching)
    }

    /// The `theirs` commit of a merge in progress (`MERGE_HEAD`), or `None`
    /// when no merge is in progress.
    pub fn merge_head(&self) -> Result<Option<Hash>, GraphError> {
        Ok(self.store.read_ref(WORKTREE_MERGE_HEAD_REF)?)
    }

    /// The conflicts of a merge in progress, in map order, or an empty vec
    /// when none remain (spec §6). Errors if no merge is in progress.
    pub fn conflicts(&self) -> Result<Vec<crate::conflicts::PersistedConflict>, GraphError> {
        if self.merge_head()?.is_none() {
            return Err(GraphError::MergeState("no merge in progress"));
        }
        let manifest = self.workspace_manifest()?;
        match manifest.conflicts {
            None => Ok(Vec::new()),
            Some(root) => {
                crate::conflicts::read_conflicts(&self.store, &root.to_root(manifest.chunk_params)?)
            }
        }
    }

    /// Resolve every cell conflict of a merge in progress by taking each
    /// conflicted key's value from one side — `ours` (the current branch) or
    /// `theirs` (`MERGE_HEAD`) — clearing the conflicts map (spec §6,
    /// `acetone resolve --all-ours|--all-theirs`). Returns the number
    /// resolved. `acetone commit` then completes the merge. Graph-level
    /// violations cannot be picked a side; resolve those by editing the graph
    /// (acetone-14c.4c).
    pub fn resolve_all(&self, side: ResolveSide) -> Result<usize, GraphError> {
        let theirs = self
            .merge_head()?
            .ok_or(GraphError::MergeState("no merge in progress"))?;
        let ours = self.head_commit()?.ok_or(GraphError::MergeState(
            "merge in progress but the branch is unborn",
        ))?;
        let source = self.manifest_at_commit(&match side {
            ResolveSide::Ours => ours,
            ResolveSide::Theirs => theirs,
        })?;
        let mut txn = self.begin_write()?;
        let count = txn.resolve_conflicts_from(&source)?;
        txn.save()?;
        Ok(count)
    }

    /// The node's record at `commit` (its non-key properties and secondary
    /// labels), or `None` when the node is absent there.
    fn record_at(&self, commit: &Hash, key: &NodeKey) -> Result<Option<NodeRecord>, GraphError> {
        let manifest = self.manifest_at_commit(commit)?;
        let root = manifest.nodes.to_root(manifest.chunk_params)?;
        match acetone_prolly::get(&self.store, &root, &key.encode()?)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(NodeRecord::decode(&bytes)?)),
        }
    }

    /// The commit history from `refspec` (default: the current branch),
    /// following first parents, newest first.
    pub fn log(&self, refspec: Option<&str>) -> Result<Vec<LogEntry>, GraphError> {
        let mut next = match refspec {
            Some(spec) => Some(self.resolve_commit(spec)?),
            None => self.head_commit()?,
        };
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            if !seen.insert(id) {
                break; // defensive: cycles cannot occur in valid git data
            }
            let commit = self
                .store
                .read_commit(&id)?
                .ok_or_else(|| GraphError::NotACommit { name: id.to_hex() })?;
            next = commit.parents.first().copied();
            entries.push(LogEntry {
                id,
                message: commit.message,
                trailers: commit.trailers,
                parents: commit.parents,
            });
        }
        Ok(entries)
    }

    /// Begin a write transaction: acquires the single-writer lock and
    /// loads the workspace manifest. All mutation and committing happens
    /// through the returned [`Transaction`].
    pub fn begin_write(&self) -> Result<Transaction<'_>, GraphError> {
        let lock = WriteLock::acquire(self.store.git_dir())?;
        // `base_ref_value` is the per-worktree ref's value now (None while
        // migrating from the legacy ref); the CAS on save expects exactly
        // this. `base_hash` is the effective manifest, legacy fallback and
        // all, so reads see the current workspace.
        let base_ref_value = self.workspace_ref_value()?;
        let base_hash = self.workspace_manifest_hash()?;
        let manifest = read_manifest_chunk(&self.store, &base_hash)?;
        Ok(Transaction {
            repo: self,
            _lock: lock,
            base_ref_value,
            base_hash,
            manifest,
            schema: Vec::new(),
            nodes: Vec::new(),
            edges_fwd: Vec::new(),
            edges_rev: Vec::new(),
        })
    }

    /// Rebuild every declared index map from the workspace `nodes`, to
    /// identical roots (spec §3.3, Invariant #5). A no-op when the indexes are
    /// already consistent. Takes the single-writer lock.
    pub fn reindex(&self) -> Result<(), GraphError> {
        self.begin_write()?.reindex()
    }

    /// Consolidate the object store into a self-contained packfile, deltaing
    /// rewritten chunks against the predecessors chosen at write time (ADR-0011,
    /// spec §3.1) and pruning the superseded loose objects and packs. This is
    /// acetone's own periodic maintenance — representation-only, preserving
    /// every object's bytes and address exactly. Takes the single-writer lock
    /// so a concurrent write's fresh loose object cannot be pruned mid-run.
    pub fn gc(&self) -> Result<ConsolidateStats, GraphError> {
        self.gc_with_hooks(|| {}, || {})
    }

    /// [`gc`](Self::gc)'s body, with two test seams for the worktree TOCTOU
    /// (acetone-dfh): `after_lock` runs once the write lock is held, *before*
    /// the under-lock linked-worktree re-check; `after_recheck` runs after the
    /// re-check passed — the residual window before the sweep. Production
    /// callers go through `gc`, which passes no-ops; the hooks let tests
    /// interleave a `git worktree add` deterministically at each point.
    #[doc(hidden)]
    pub fn gc_with_hooks(
        &self,
        after_lock: impl FnOnce(),
        after_recheck: impl FnOnce(),
    ) -> Result<ConsolidateStats, GraphError> {
        // Consolidation's reachability walk (`references().all()`) does not see
        // *other* linked worktrees' private refs (`refs/worktree/*`, ADR-0014),
        // so pruning could destroy their uncommitted workspace or in-progress
        // merge. Refuse when any linked worktree exists until gc is made
        // worktree-aware (walk every worktree's private refs); the single-
        // worktree case — the common one — is safe. This pre-lock check is the
        // cheap fast-fail for the common error path only; the authoritative
        // check is the one below, under the lock.
        if self.has_linked_worktrees()? {
            return Err(GraphError::GcWithLinkedWorktrees);
        }
        let _lock = WriteLock::acquire(self.store.git_dir())?;
        after_lock();
        // Re-check under the write lock (acetone-dfh): a `git worktree add`
        // racing the pre-lock check would otherwise slip past the refusal and
        // have its private state swept. The lock does not stop `git worktree
        // add` itself (plain git respects no acetone lock) — it merely
        // re-anchors the check as late as possible; the sweep below is
        // additionally safe for a worktree that appears later still.
        if self.has_linked_worktrees()? {
            return Err(GraphError::GcWithLinkedWorktrees);
        }
        after_recheck();
        // gc runs only when no linked worktree was seen (checked above), so
        // `refs/acetone/worktree-anchors/*` are leftovers from since-removed
        // worktrees — pinning chunks nothing live needs (ADR-0044). Delete
        // them before consolidating so their now-unreferenced chunks are
        // reclaimed. Each deletion is gated on the anchor's worktree directory
        // being absent AT DELETION TIME: a worktree that appears in the window
        // after the re-check keeps its durability anchor — when in doubt, gc
        // keeps data. (A crafted anchor id could only make the existence check
        // succeed, i.e. keep the anchor; deletion goes through the ref store,
        // never a raw path.)
        let worktrees_dir = self.store.common_dir().join("worktrees");
        for (anchor, _) in self.store.list_refs(WORKTREE_ANCHOR_PREFIX)? {
            let id = anchor
                .strip_prefix(WORKTREE_ANCHOR_PREFIX)
                .unwrap_or(&anchor);
            if worktrees_dir.join(id).exists() {
                continue; // a worktree appeared mid-gc: its anchor is live
            }
            self.store.delete_ref(&anchor)?;
        }
        // Reading B (ADR-0051): pack only objects reachable from refs this graph
        // owns; a co-tenant's code refs form a prune guard so their objects are
        // preserved and their storage left untouched. In the standalone layout
        // the namespace owns every ref, so this is the whole reachable set.
        //
        // A worktree appearing during consolidation itself loses nothing:
        // pruning only ever removes a loose copy of an object already in the
        // durably installed pack, and a prior pack only once every object it
        // indexes is in the new pack — so no step here deletes the last copy
        // of anything a late worktree references. Chunks it writes are new
        // loose objects outside the packed set, untouched by pruning.
        let namespace = &self.namespace;
        Ok(self
            .store
            .consolidate_scoped(ConsolidateOptions::default(), &|name| {
                namespace.owns_ref(name)
            })?)
    }

    /// Whether the repository has any linked worktree — git records one
    /// directory per linked worktree under `<common>/worktrees/`.
    fn has_linked_worktrees(&self) -> Result<bool, GraphError> {
        let dir = self.store.common_dir().join("worktrees");
        match std::fs::read_dir(&dir) {
            Ok(mut entries) => Ok(entries.next().is_some()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(GraphError::LockIo { path: dir, source }),
        }
    }

    /// Resolve a refspec — branch short name, full ref name, or hex
    /// commit hash — to a commit address.
    ///
    /// A refspec that fails one interpretation (not a valid ref name, a
    /// hex address naming a non-commit object) simply falls through to
    /// the next and ultimately to [`GraphError::UnresolvedRefspec`];
    /// genuine store damage still surfaces as its own error.
    pub fn resolve_commit(&self, refspec: &str) -> Result<Hash, GraphError> {
        let as_branch = self.namespace.branch_ref(refspec);
        if let Some(hash) = read_ref_lenient(&self.store, &as_branch)? {
            return Ok(hash);
        }
        if refspec.starts_with("refs/")
            && let Some(hash) = read_ref_lenient(&self.store, refspec)?
        {
            return Ok(hash);
        }
        if let Ok(hash) = Hash::from_hex(refspec) {
            match self.store.read_commit(&hash) {
                Ok(Some(_)) => return Ok(hash),
                // A hex address naming nothing, or a non-commit object,
                // is an unresolvable refspec, not a store failure.
                Ok(None) | Err(StoreError::WrongObjectKind { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(GraphError::UnresolvedRefspec {
            refspec: refspec.to_owned(),
        })
    }

    /// The manifest blob hash inside a commit (re-adding the manifest
    /// bytes to the content-addressed store is the identity operation, so
    /// this needs no extra bookkeeping).
    pub(crate) fn commit_manifest_hash(&self, commit: &Hash) -> Result<Hash, GraphError> {
        let commit = self
            .store
            .read_commit(commit)?
            .ok_or_else(|| GraphError::NotACommit {
                name: commit.to_hex(),
            })?;
        Ok(self.store.put(&commit.manifest)?)
    }

    /// The manifest of the version at `commit` (its manifest blob, decoded).
    fn manifest_at_commit(&self, commit: &Hash) -> Result<Manifest, GraphError> {
        read_manifest_chunk(&self.store, &self.commit_manifest_hash(commit)?)
    }

    /// Advance `branch` to `target` and reset the workspace to match — the
    /// fast-forward path (and the unborn-branch adoption, with
    /// `expected = None`). No merge commit is created; `target` already
    /// contains our history.
    fn fast_forward(
        &self,
        branch: &str,
        expected: Option<&Hash>,
        target: &Hash,
    ) -> Result<Hash, GraphError> {
        let manifest_hash = self.commit_manifest_hash(target)?;
        let tree = self.workspace_tree_for(&manifest_hash)?;
        let ws_expected = self.workspace_ref_value()?;
        self.cas_workspace(ws_expected.as_ref(), &tree)?;
        match self.store.write_ref(branch, expected, target) {
            Ok(()) => Ok(*target),
            Err(StoreError::CasFailed { .. }) => Err(GraphError::BranchConflict {
                name: branch.to_owned(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Write a merge commit over `manifest` with the given `parents`,
    /// anchoring the manifest's complete chunk set (like
    /// `Transaction::commit`, but with an explicit manifest and parent list
    /// rather than the workspace manifest and the single branch tip). Does
    /// not touch any ref — the caller advances the branch.
    fn write_merge_commit(
        &self,
        manifest: &Manifest,
        parents: &[Hash],
        message: &str,
    ) -> Result<Hash, GraphError> {
        let manifest_bytes = manifest.encode();
        let anchors = manifest_chunk_set(&self.store, manifest)?;
        let summary = summarise(&self.store, manifest)?;
        let mut new_commit = NewCommit::new(&manifest_bytes, &summary, message);
        new_commit.parents = parents;
        new_commit.anchors = &anchors;
        Ok(self.store.create_commit(&new_commit)?)
    }

    /// Every commit reachable from `start` by following parents, `start`
    /// included. Bounded by history size; valid git data has no cycles, and
    /// the visited set makes a corrupt one terminate rather than loop.
    fn ancestors(&self, start: &Hash) -> Result<BTreeSet<Hash>, GraphError> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![*start];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let commit = self
                .store
                .read_commit(&id)?
                .ok_or_else(|| GraphError::NotACommit { name: id.to_hex() })?;
            stack.extend(commit.parents);
        }
        Ok(seen)
    }

    /// Whether `anc` is an ancestor of `desc` (reflexive: true when equal).
    /// Walks `desc`'s ancestry, short-circuiting when `anc` is reached.
    fn is_ancestor(&self, anc: &Hash, desc: &Hash) -> Result<bool, GraphError> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![*desc];
        while let Some(id) = stack.pop() {
            if &id == anc {
                return Ok(true);
            }
            if !seen.insert(id) {
                continue;
            }
            let commit = self
                .store
                .read_commit(&id)?
                .ok_or_else(|| GraphError::NotACommit { name: id.to_hex() })?;
            stack.extend(commit.parents);
        }
        Ok(false)
    }

    /// A merge base (lowest common ancestor) of `a` and `b` over the commit
    /// DAG, or `None` when they share no ancestor (unrelated histories).
    ///
    /// Computed as the maximal elements of the common-ancestor set: a common
    /// ancestor that is itself a proper ancestor of another common ancestor
    /// is not lowest and is dropped. For the ordinary (non-criss-cross)
    /// topology this leaves exactly one commit. On a criss-cross history
    /// several maximal common ancestors can remain; any is a valid base and
    /// the three-way merge over it is deterministic, so the smallest by hash
    /// is chosen for a stable choice. This keeps the merge reproducible
    /// *within a repository* (which is all Invariant #4 requires — merge is a
    /// pure function of the chosen base); note the tie-break is over the
    /// commit hash, which embeds a wall-clock timestamp, so two repositories
    /// built from the same logical criss-cross history could pick different
    /// bases. This is not git's recursive-merge "virtual base" — a documented
    /// simplification for v0.1 (spec §7).
    ///
    /// Public so a merge inspector can re-derive the three-way base of an
    /// in-progress merge (`acetone.conflicts`, acetone-s7d): the base's values
    /// are recomputed from `merge_base(ours, MERGE_HEAD)`, not persisted.
    pub fn merge_base(&self, a: &Hash, b: &Hash) -> Result<Option<Hash>, GraphError> {
        let anc_a = self.ancestors(a)?;
        let anc_b = self.ancestors(b)?;
        let common: Vec<Hash> = anc_a.intersection(&anc_b).copied().collect();
        if common.is_empty() {
            return Ok(None);
        }
        let mut bases: Vec<Hash> = Vec::new();
        for c in &common {
            let mut dominated = false;
            for d in &common {
                if d != c && self.is_ancestor(c, d)? {
                    dominated = true;
                    break;
                }
            }
            if !dominated {
                bases.push(*c);
            }
        }
        // A finite non-empty DAG always has a maximal element, so `bases` is
        // non-empty here; `min` is a total, deterministic tie-break.
        Ok(bases.into_iter().min())
    }

    /// Compare-and-swap the per-worktree workspace ref. `expected` is its
    /// value at transaction begin — `None` creates it (first write after an
    /// ADR-0014 migration, or a fresh transaction whose ref was absent).
    ///
    /// Every workspace advance funnels through here (init bootstrap,
    /// transaction save, merge, abort, reindex), so this is also where a
    /// *linked* worktree renews its common-dir durability anchor (ADR-0044):
    /// once the per-worktree CAS has committed the new workspace tree, mirror
    /// it into `refs/acetone/worktree-anchors/<id>` so a foreign `git gc` from
    /// the main worktree cannot prune the linked worktree's uncommitted chunks.
    /// The anchor merely follows the workspace tree, so it is force-written; a
    /// failure to anchor fails the save (durability is the whole point).
    fn cas_workspace(&self, expected: Option<&Hash>, new: &Hash) -> Result<(), GraphError> {
        match self.store.write_ref(WORKTREE_WORKSPACE_REF, expected, new) {
            Ok(()) => {}
            Err(StoreError::CasFailed { .. }) => {
                return Err(GraphError::WorkspaceConflict {
                    name: self.workspace.clone(),
                });
            }
            Err(e) => return Err(e.into()),
        }
        if let Some(anchor) = self.worktree_anchor_ref()? {
            self.store.overwrite_ref(&anchor, new)?;
        }
        Ok(())
    }

    /// The common-dir durability anchor ref for *this* worktree, or `None` for
    /// the main worktree (whose `refs/worktree/*` workspace ref already lives
    /// in the common store and is gc-enumerated — it needs no anchor). A linked
    /// worktree's git dir is `<common>/worktrees/<id>`; its basename is the
    /// stable worktree id git itself uses, and keys the anchor (ADR-0044).
    ///
    /// A linked worktree whose id is not usable as a ref-name component is an
    /// error, not a silent `None`: skipping the anchor would quietly drop this
    /// worktree's foreign-gc durability. (The narrower "id is not valid UTF-8"
    /// case is caught here; a UTF-8 id that still fails git's ref-name rules is
    /// caught downstream by `overwrite_ref`'s `validated_ref_name`.)
    fn worktree_anchor_ref(&self) -> Result<Option<String>, GraphError> {
        let git_dir = self.store.git_dir();
        if git_dir == self.store.common_dir() {
            return Ok(None); // main worktree — no anchor needed
        }
        match git_dir.file_name().and_then(|id| id.to_str()) {
            Some(id) => Ok(Some(format!("{WORKTREE_ANCHOR_PREFIX}{id}"))),
            None => Err(GraphError::WorktreeAnchorUnnameable {
                id: git_dir
                    .file_name()
                    .unwrap_or(git_dir.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
            }),
        }
    }
}

/// Render a node key's values for an error message (`[a, 1]`-style),
/// escaping string values so attacker-writable keys cannot leak terminal
/// control sequences or Rust `Debug` internals.
fn render_node_key(key: &NodeKey) -> String {
    acetone_model::display::format_key_tuple(key.key())
}

/// Reject a schema change that alters a label's key tuple while nodes bearing
/// that label already exist in `base_nodes` (Invariant #3). Node identity is
/// `(primary label, key tuple)`, so changing the key would orphan every existing
/// node's key from the schema — an unsupported mutation (`migrate` is the path).
fn check_label_key_stability(
    store: &GitStore,
    params: ChunkParams,
    base_nodes: &MapRoot,
    old_keys: &BTreeMap<String, Vec<String>>,
    new_keys: &BTreeMap<String, Vec<String>>,
) -> Result<(), GraphError> {
    let root = base_nodes.to_root(params)?;
    for (label, new_key) in new_keys {
        // A label absent from the old schema is a fresh declaration, not a
        // change; an unchanged key tuple is fine.
        match old_keys.get(label) {
            Some(old_key) if old_key != new_key => {}
            _ => continue,
        }
        // The key tuple changed: reject if any pre-existing node bears this
        // label. Its encoded key is prefixed by the label, so a single
        // prefix scan decides existence.
        let prefix = acetone_model::graph_keys::node_label_prefix(label);
        if let Some(item) = acetone_prolly::scan(
            store,
            &root,
            (Bound::Included(prefix.as_slice()), Bound::Unbounded),
        )?
        .next()
        {
            let (key, _) = item?;
            if key.starts_with(&prefix) {
                return Err(GraphError::LabelKeyChanged {
                    label: label.clone(),
                });
            }
        }
    }
    Ok(())
}

/// A `GraphError::DanglingEdge` naming the offending edge and missing endpoint.
fn dangling_edge(rtype: &str, role: &'static str, endpoint: &NodeKey) -> GraphError {
    GraphError::DanglingEdge {
        rtype: rtype.to_owned(),
        role,
        endpoint: acetone_model::display::format_node_identity(endpoint.label(), endpoint.key()),
    }
}

/// Enforce referential integrity for one transaction's staged changes against
/// its resulting map roots (ADR-0028, Invariant #3). An edge must never exist
/// without both its endpoint nodes. A save can break this in two ways: by
/// putting an edge whose `src` or `dst` node is absent from the new `nodes`
/// map, or by deleting a node while an edge still references it. Both are
/// checked against the post-transaction roots, so the check is correct
/// regardless of op order within the transaction. It is incremental: only the
/// edges added and nodes deleted this transaction are examined, and a deleted
/// node's incident edges are a degree-bounded prefix scan (its key bytes are
/// exactly the prefix of every edge whose leading endpoint is that node —
/// out-edges in `edges_fwd`, in-edges in `edges_rev`).
fn check_referential_integrity(
    store: &GitStore,
    params: ChunkParams,
    nodes: &MapRoot,
    edges_fwd: &MapRoot,
    edges_rev: &MapRoot,
    put_edge_keys: &[Vec<u8>],
    deleted_node_keys: &[Vec<u8>],
) -> Result<(), GraphError> {
    let nodes_root = nodes.to_root(params)?;

    // (1) Every edge added this transaction must have both endpoints present.
    for raw in put_edge_keys {
        let edge = EdgeKey::decode_fwd(raw)?;
        for (role, endpoint) in [("source", edge.src()), ("target", edge.dst())] {
            if acetone_prolly::get(store, &nodes_root, &endpoint.encode()?)?.is_none() {
                return Err(dangling_edge(edge.rtype(), role, endpoint));
            }
        }
    }

    // (2) Every node genuinely deleted this transaction must have no surviving
    // incident edge.
    if !deleted_node_keys.is_empty() {
        let fwd_root = edges_fwd.to_root(params)?;
        let rev_root = edges_rev.to_root(params)?;
        for key in deleted_node_keys {
            // A `Delete` op only dangles if the node really is gone from the
            // new map (a re-add in the same transaction is not a deletion).
            if acetone_prolly::get(store, &nodes_root, key)?.is_some() {
                continue;
            }
            let node = NodeKey::decode(key)?;
            // `edges_fwd` is keyed by src, `edges_rev` by dst; in both the
            // leading endpoint's key is the prefix, so the deleted node's key
            // bytes select exactly its out-edges (fwd) and in-edges (rev).
            for (root, reversed) in [(&fwd_root, false), (&rev_root, true)] {
                let hit = acetone_prolly::scan(
                    store,
                    root,
                    (Bound::Included(key.as_slice()), Bound::Unbounded),
                )?
                .next();
                if let Some(item) = hit {
                    let (edge_key, _) = item?;
                    if edge_key.starts_with(key) {
                        let edge = if reversed {
                            EdgeKey::decode_rev(&edge_key)?
                        } else {
                            EdgeKey::decode_fwd(&edge_key)?
                        };
                        // The deleted node is this edge's leading endpoint: its
                        // source for a forward hit, its target for a reverse.
                        let role = if reversed { "target" } else { "source" };
                        return Err(dangling_edge(edge.rtype(), role, &node));
                    }
                }
            }
        }
    }
    Ok(())
}

fn workspace_ref(name: &str) -> String {
    format!("{WORKSPACE_REF_PREFIX}{name}")
}

/// Write the empty-graph workspace: build the empty manifest under
/// `chunk_params`, anchor its chunk set in a workspace tree, and point the
/// per-worktree workspace ref at it. Shared by [`Repository::init`] (standalone)
/// and [`Repository::init_co_tenant`]; the two differ only in the ref layout
/// they then set up, not in the empty graph they start from.
fn provision_empty_workspace(
    store: &GitStore,
    chunk_params: ChunkParams,
) -> Result<(), GraphError> {
    let empty = acetone_prolly::empty(store, chunk_params)?;
    let manifest = Manifest {
        chunk_params,
        schema: MapRoot::from_root(&empty),
        nodes: MapRoot::from_root(&empty),
        edges_fwd: MapRoot::from_root(&empty),
        edges_rev: MapRoot::from_root(&empty),
        indexes: Default::default(),
        conflicts: None,
    };
    // The workspace ref points at a workspace tree that anchors the manifest's
    // chunk set, so uncommitted state survives a foreign gc (huo). For the
    // empty graph that is just the empty prolly root.
    let anchors = manifest_chunk_set(store, &manifest)?;
    let tree = store.write_workspace_tree(&manifest.encode(), &anchors)?;
    store.write_ref(WORKTREE_WORKSPACE_REF, None, &tree)?;
    Ok(())
}

/// Detect a repository's ref layout from its co-tenant graph markers
/// (ADR-0050): no marker ⇒ standalone; exactly one ⇒ co-tenant for that graph.
/// More than one is [`GraphError::MultipleGraphs`] (multi-graph selection is
/// deferred). The marker is a *direct* ref, so `list_refs` enumerates it.
///
/// The marker-derived name is **re-validated** before it shapes a namespace
/// (acetone-c2a): `init_co_tenant` validates the name it writes, but a marker
/// is just a ref in `.git`, so a hand-crafted one (e.g.
/// `refs/acetone/graphs/a/b`, a valid git ref whose stripped name would split
/// the namespace) could otherwise smuggle in an arbitrary layout. A marker
/// that fails validation is [`GraphError::InvalidGraphName`] — a loud refusal
/// to open, never a silently odd namespace.
fn detect_namespace(store: &GitStore) -> Result<GraphRefNamespace, GraphError> {
    let markers = store.list_refs(GRAPHS_REF_PREFIX)?;
    match markers.as_slice() {
        [] => Ok(GraphRefNamespace::standalone()),
        [(name, _)] => {
            let graph = name.strip_prefix(GRAPHS_REF_PREFIX).unwrap_or(name);
            validate_graph_name(graph)?;
            Ok(GraphRefNamespace::co_tenant(graph))
        }
        _ => Err(GraphError::MultipleGraphs {
            names: markers
                .into_iter()
                .map(|(name, _)| {
                    name.strip_prefix(GRAPHS_REF_PREFIX)
                        .unwrap_or(&name)
                        .to_owned()
                })
                .collect(),
        }),
    }
}

/// Validate a co-tenant graph name: it namespaces the graph's refs
/// (`refs/heads/acetone/<name>/*`, ADR-0050), so it must be a single, well-formed
/// ref-path component. Rejects the empty string, any `/` (which would split the
/// namespace), `..` (traversal-shaped), a leading `.` or a trailing `.lock`
/// (git ref-format rules), and ASCII control/space/special characters git
/// forbids in ref components. The store door (`validated_ref_name`) is the final
/// backstop; this keeps the rejection close to the caller.
fn validate_graph_name(graph: &str) -> Result<(), GraphError> {
    let reject = |reason: &'static str| {
        Err(GraphError::InvalidGraphName {
            name: graph.to_owned(),
            reason,
        })
    };
    if graph.is_empty() {
        return reject("empty");
    }
    if graph.contains('/') {
        return reject("must be a single ref-path component (no '/')");
    }
    if graph == "." || graph == ".." || graph.contains("..") {
        return reject("must not contain '..'");
    }
    if graph.starts_with('.') {
        return reject("must not start with '.'");
    }
    if graph.ends_with(".lock") {
        return reject("must not end with '.lock'");
    }
    if graph.chars().any(|c| {
        c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return reject("contains a character git forbids in a ref component");
    }
    Ok(())
}

/// The key of a staged batch op (present on both `Put` and `Delete`).
fn batch_op_key(op: &BatchOp) -> &[u8] {
    match op {
        BatchOp::Put(key, _) => key,
        BatchOp::Delete(key) => key,
    }
}

/// Read a ref for refspec resolution: a name that is not a valid direct
/// ref (invalid format, symbolic) reads as absent rather than an error,
/// so resolution can fall through to the next interpretation.
fn read_ref_lenient(store: &GitStore, name: &str) -> Result<Option<Hash>, GraphError> {
    match store.read_ref(name) {
        Ok(found) => Ok(found),
        Err(StoreError::InvalidRefName { .. } | StoreError::SymbolicRef { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Render chunk parameters for the [`GraphError::ChunkParamsMismatch`]
/// message.
fn render_chunk_params(params: ChunkParams) -> String {
    format!(
        "(min_bytes {}, mask_bits {}, max_bytes {})",
        params.min_bytes(),
        params.mask_bits(),
        params.max_bytes()
    )
}

pub(crate) fn read_manifest_chunk(store: &GitStore, hash: &Hash) -> Result<Manifest, GraphError> {
    let bytes = store.get(hash)?.ok_or_else(|| StoreError::InvalidHash {
        reason: format!("workspace manifest chunk {hash} is missing"),
    })?;
    Ok(Manifest::decode(&bytes)?)
}

/// A write transaction: staged mutations against the workspace, applied
/// atomically by [`Transaction::save`] (or [`Transaction::commit`]).
/// Holds the single-writer lock for its lifetime.
///
/// Mutations are **raw map plumbing** in Phase 1: no schema validation,
/// no constraint checks, no index maintenance (the `indexes` map rides
/// along unchanged) — those arrive with the graph-semantics beads. What
/// *is* enforced by construction: `edges_rev` is updated in the same
/// atomic save as `edges_fwd` (spec §3.3).
#[derive(Debug)]
pub struct Transaction<'r> {
    repo: &'r Repository,
    _lock: WriteLock,
    /// The per-worktree workspace ref's value at begin (the CAS expected;
    /// `None` while migrating from the legacy ref).
    base_ref_value: Option<Hash>,
    base_hash: Hash,
    manifest: Manifest,
    schema: Vec<BatchOp>,
    nodes: Vec<BatchOp>,
    edges_fwd: Vec<BatchOp>,
    edges_rev: Vec<BatchOp>,
}

impl<'r> Transaction<'r> {
    /// Insert or replace a node.
    pub fn put_node(&mut self, key: &NodeKey, record: &NodeRecord) -> Result<(), GraphError> {
        self.nodes
            .push(BatchOp::Put(key.encode()?, record.encode()?));
        Ok(())
    }

    /// Delete a node if present. (Plumbing: does not touch edges — the
    /// graph-semantics layer will handle cascades and constraints.)
    pub fn delete_node(&mut self, key: &NodeKey) -> Result<(), GraphError> {
        self.nodes.push(BatchOp::Delete(key.encode()?));
        Ok(())
    }

    /// Insert or replace an edge, maintaining both edge maps.
    pub fn put_edge(&mut self, key: &EdgeKey, record: &EdgeRecord) -> Result<(), GraphError> {
        self.edges_fwd
            .push(BatchOp::Put(key.encode_fwd()?, record.encode()?));
        self.edges_rev
            .push(BatchOp::Put(key.encode_rev()?, Vec::new()));
        Ok(())
    }

    /// Delete an edge if present, from both edge maps.
    pub fn delete_edge(&mut self, key: &EdgeKey) -> Result<(), GraphError> {
        self.edges_fwd.push(BatchOp::Delete(key.encode_fwd()?));
        self.edges_rev.push(BatchOp::Delete(key.encode_rev()?));
        Ok(())
    }

    /// Insert or replace a schema entry.
    pub fn put_schema(&mut self, entry: &SchemaEntry) -> Result<(), GraphError> {
        self.schema
            .push(BatchOp::Put(entry.map_key(), entry.encode_value()));
        Ok(())
    }

    /// Whether any mutations are staged.
    pub fn is_dirty(&self) -> bool {
        !(self.schema.is_empty()
            && self.nodes.is_empty()
            && self.edges_fwd.is_empty()
            && self.edges_rev.is_empty())
    }

    /// Apply staged ops to one map, returning the new root and the
    /// `(new_chunk, predecessor)` base hints the splice discovered — recorded
    /// for a later `gc` so rewritten chunks delta against their predecessors
    /// (ADR-0011). The root is identical to a plain `apply_batch`.
    fn apply_map(
        store: &GitStore,
        params: ChunkParams,
        root: &MapRoot,
        ops: Vec<BatchOp>,
    ) -> Result<MapRoot, GraphError> {
        if ops.is_empty() {
            return Ok(*root);
        }
        let root = root.to_root(params)?;
        // Record the `(new_chunk, predecessor)` base hints the splice discovers
        // (a local sidecar, never transferred) so a later `gc` deltas rewritten
        // chunks against their predecessors (ADR-0011). The root is identical to
        // a plain `apply_batch`; losing hints only makes gc store more whole.
        let (new_root, hints) = acetone_prolly::apply_batch_recording(store, &root, ops)?;
        // Best-effort: the hints are a local gc optimisation, so a failed write
        // to the sidecar must never fail an otherwise-valid commit — losing
        // them only makes a later gc store more objects whole.
        if !hints.is_empty() {
            let _ = store.record_base_hints(&hints);
        }
        Ok(MapRoot::from_root(&new_root))
    }

    /// Apply the staged mutations: new map roots, new manifest chunk,
    /// atomic workspace advance (compare-and-swap against the manifest
    /// this transaction loaded). Returns the new manifest.
    pub fn save(mut self) -> Result<Manifest, GraphError> {
        self.save_in_place()?;
        Ok(self.manifest)
    }

    fn save_in_place(&mut self) -> Result<(), GraphError> {
        if !self.is_dirty() {
            return Ok(());
        }
        let store = &self.repo.store;
        let params = self.manifest.chunk_params;
        // Captured before the staged node ops are consumed below, for derived
        // index maintenance (spec §3.3): the pre-transaction `nodes` root, the
        // node keys written this save, the pre-transaction index roots, and
        // whether the schema changed (a new index may have been declared).
        let base_nodes = self.manifest.nodes;
        let base_indexes = self.manifest.indexes.clone();
        let touched_nodes: Vec<Vec<u8>> = self
            .nodes
            .iter()
            .map(|op| batch_op_key(op).to_vec())
            .collect();
        let schema_changed = !self.schema.is_empty();
        // By-write conflict resolution (spec §6, acetone-14c.4c): while a merge
        // is in progress, writing a conflicted key resolves it. Collect the
        // keys written this save (before the ops are consumed) so the
        // conflicts map can be reduced by them below.
        let written: Vec<(ConflictMap, Vec<u8>)> = if self.manifest.conflicts.is_some() {
            let mut w = Vec::new();
            for op in &self.schema {
                w.push((ConflictMap::Schema, batch_op_key(op).to_vec()));
            }
            for op in &self.nodes {
                w.push((ConflictMap::Nodes, batch_op_key(op).to_vec()));
            }
            for op in &self.edges_fwd {
                w.push((ConflictMap::Edges, batch_op_key(op).to_vec()));
            }
            w
        } else {
            Vec::new()
        };
        let conflicts = match self.manifest.conflicts {
            Some(root) if !written.is_empty() => {
                crate::conflicts::clear_written(store, &root.to_root(params)?, &written)?
                    .map(|r| MapRoot::from_root(&r))
            }
            other => other,
        };
        // For the referential-integrity check (ADR-0028), captured before the
        // staged ops are consumed below: forward-edge keys added, and node keys
        // deleted, this transaction.
        let put_edge_keys: Vec<Vec<u8>> = self
            .edges_fwd
            .iter()
            .filter_map(|op| match op {
                BatchOp::Put(k, _) => Some(k.clone()),
                BatchOp::Delete(_) => None,
            })
            .collect();
        let deleted_node_keys: Vec<Vec<u8>> = self
            .nodes
            .iter()
            .filter_map(|op| match op {
                BatchOp::Delete(k) => Some(k.clone()),
                BatchOp::Put(..) => None,
            })
            .collect();
        let mut manifest = Manifest {
            chunk_params: params,
            schema: Self::apply_map(
                store,
                params,
                &self.manifest.schema,
                std::mem::take(&mut self.schema),
            )?,
            nodes: Self::apply_map(
                store,
                params,
                &self.manifest.nodes,
                std::mem::take(&mut self.nodes),
            )?,
            edges_fwd: Self::apply_map(
                store,
                params,
                &self.manifest.edges_fwd,
                std::mem::take(&mut self.edges_fwd),
            )?,
            edges_rev: Self::apply_map(
                store,
                params,
                &self.manifest.edges_rev,
                std::mem::take(&mut self.edges_rev),
            )?,
            indexes: base_indexes.clone(),
            conflicts,
        };
        // Guard node identity (Invariant #3): a schema change must not alter a
        // label's key tuple while nodes bearing that label already exist — that
        // would orphan every existing node's key from the schema. Such a change
        // needs an explicit `migrate`, not a redeclare.
        if schema_changed {
            let old_keys = crate::index::schema_index_info(
                &Snapshot::new(store, self.manifest.clone()).schema_entries()?,
            )
            .1;
            let new_keys = crate::index::schema_index_info(
                &Snapshot::new(store, manifest.clone()).schema_entries()?,
            )
            .1;
            check_label_key_stability(store, params, &base_nodes, &old_keys, &new_keys)?;
        }
        // Maintain the derived `idx/<name>` maps (spec §3.3, Invariant #5).
        // Skipped entirely when no index exists and the schema is unchanged, so
        // the common index-free write path costs nothing.
        if !base_indexes.is_empty() || schema_changed {
            let entries = Snapshot::new(store, manifest.clone()).schema_entries()?;
            let (index_defs, label_keys) = crate::index::schema_index_info(&entries);
            // On a schema change an index's *definition* may have changed
            // (redeclaration under the same name). Incrementally deltaing the new
            // definition onto the map built for the old one leaves stale/wrong
            // entries, so a redefined index must be rebuilt from scratch: drop
            // its base root, which sends `maintain` down the full-build path.
            // Indexes whose definition is unchanged keep their base root and stay
            // cheap-incremental. (Only possible on a schema change, so the common
            // path is untouched.)
            let base_for_maintain = if schema_changed {
                let old_entries = Snapshot::new(store, self.manifest.clone()).schema_entries()?;
                let old_defs: BTreeMap<String, IndexDef> =
                    crate::index::schema_index_info(&old_entries)
                        .0
                        .into_iter()
                        .collect();
                let new_defs: BTreeMap<String, IndexDef> = index_defs.iter().cloned().collect();
                base_indexes
                    .iter()
                    .filter(|(name, _)| old_defs.get(*name) == new_defs.get(*name))
                    .map(|(k, v)| (k.clone(), *v))
                    .collect()
            } else {
                base_indexes.clone()
            };
            manifest.indexes = crate::index::maintain(
                store,
                params,
                &base_nodes,
                &manifest.nodes,
                &touched_nodes,
                &base_for_maintain,
                &index_defs,
                &label_keys,
            )?;
        }
        // Referential integrity (ADR-0028): reject before the workspace CAS
        // advances if this transaction would leave any edge without an endpoint
        // node — an edge put whose endpoint is absent, or a node deleted while
        // an edge still references it.
        if !put_edge_keys.is_empty() || !deleted_node_keys.is_empty() {
            check_referential_integrity(
                store,
                params,
                &manifest.nodes,
                &manifest.edges_fwd,
                &manifest.edges_rev,
                &put_edge_keys,
                &deleted_node_keys,
            )?;
        }
        self.persist_manifest(manifest)
    }

    /// Write a new manifest chunk and atomically advance the workspace ref to a
    /// fresh tree anchoring its chunk set (compare-and-swap against the tree
    /// this transaction loaded). Updates the transaction's base state.
    fn persist_manifest(&mut self, manifest: Manifest) -> Result<(), GraphError> {
        let store = &self.repo.store;
        let manifest_bytes = manifest.encode();
        let new_manifest_hash = store.put(&manifest_bytes)?;
        // Advance the per-worktree ref to a fresh workspace tree that
        // anchors the new manifest's chunk set (huo). Skip only when the
        // manifest is unchanged *and* the ref already exists as a tree
        // (i.e. not a pending pre-huo/migration write).
        if new_manifest_hash != self.base_hash || self.base_ref_value.is_none() {
            let anchors = manifest_chunk_set(store, &manifest)?;
            let tree = store.write_workspace_tree(&manifest_bytes, &anchors)?;
            self.repo
                .cas_workspace(self.base_ref_value.as_ref(), &tree)?;
            self.base_ref_value = Some(tree);
        }
        self.base_hash = new_manifest_hash;
        self.manifest = manifest;
        Ok(())
    }

    /// Rebuild every declared index map from the workspace `nodes` and persist
    /// the result (spec §3.3, Invariant #5: `reindex` reproduces identical
    /// roots). A no-op — same manifest hash — when the indexes are already
    /// consistent.
    pub fn reindex(mut self) -> Result<(), GraphError> {
        let store = &self.repo.store;
        let entries = Snapshot::new(store, self.manifest.clone()).schema_entries()?;
        let indexes = crate::index::rebuild_all(store, &self.manifest, &entries)?;
        let manifest = Manifest {
            indexes,
            ..self.manifest.clone()
        };
        self.persist_manifest(manifest)
    }

    /// Save any staged mutations, then turn the workspace manifest into a
    /// git commit on the current branch and advance the branch ref.
    /// Returns the commit's address.
    ///
    /// The commit anchors the complete chunk set of every map in the
    /// manifest, so the whole version survives `git gc` and travels with
    /// `clone`/`push`/`fetch` (spec §3.5).
    pub fn commit(
        mut self,
        message: &str,
        trailers: &[(String, String)],
        author: Option<Signature>,
    ) -> Result<Hash, GraphError> {
        let repo = self.repo;
        // Committing advances a branch, so a detached-HEAD worktree cannot
        // commit. Resolve the branch *before* applying staged writes, so a
        // detached commit fails cleanly (`NoCurrentBranch`) without partially
        // mutating the workspace (acetone-cm9 review).
        let branch = repo.current_branch()?.ok_or(GraphError::NoCurrentBranch)?;
        // Defence in depth (acetone-093, Gate D freeze audit): chunking
        // parameters are fixed at init (spec §3.2) and propagate through
        // every manifest, so this commit's params must equal its parent's.
        // Checked before applying staged writes, so a mismatched
        // transaction fails cleanly without mutating the workspace. The
        // first commit on an unborn branch has no parent to agree with.
        if let Some(parent) = repo.store.read_ref(&branch)? {
            let parent_params = repo.manifest_at_commit(&parent)?.chunk_params;
            if parent_params != self.manifest.chunk_params {
                return Err(GraphError::ChunkParamsMismatch {
                    expected: render_chunk_params(parent_params),
                    actual: render_chunk_params(self.manifest.chunk_params),
                });
            }
        }
        // A merge in progress (MERGE_HEAD set) completes here: the commit gets
        // `theirs` as a second parent (spec §6). It may only complete once
        // every conflict is resolved. Apply staged writes first, so a write
        // that resolves the last conflict in this same transaction is seen
        // (14c.4c) before the unresolved-conflicts check.
        let merge_head = repo.store.read_ref(WORKTREE_MERGE_HEAD_REF)?;
        self.save_in_place()?;

        let parent = repo.store.read_ref(&branch)?;

        // Defensive (acetone-mws, m2): a MERGE_HEAD already in the branch tip's
        // history is stale — a prior completion whose `delete_ref` failed. Do
        // not add it as a second parent; clear it and commit as an ordinary
        // single-parent commit.
        let merge_head = match (merge_head, parent) {
            (Some(theirs), Some(tip)) if repo.is_ancestor(&theirs, &tip)? => {
                repo.store.delete_ref(WORKTREE_MERGE_HEAD_REF)?;
                None
            }
            (mh, _) => mh,
        };

        if let Some(theirs) = merge_head {
            // Completing a genuine merge. Every cell conflict must be resolved
            // (each resolving write clears its entry via `clear_written`); a
            // remaining cell entry means unresolved work. Graph-violation entries
            // are never cleared by a write — they are resolved by repairing the
            // graph, and cleared here once the resolved graph re-validates.
            let remaining = match self.manifest.conflicts {
                Some(root) => crate::conflicts::read_conflicts(
                    &repo.store,
                    &root.to_root(self.manifest.chunk_params)?,
                )?,
                None => Vec::new(),
            };
            if remaining
                .iter()
                .any(|c| matches!(c, crate::conflicts::PersistedConflict::Cell { .. }))
            {
                return Err(GraphError::MergeState(
                    "cannot commit: unresolved merge conflicts remain — resolve them first",
                ));
            }
            // Re-validate the resolved graph against the merge base
            // (acetone-mws / acetone-36y): a resolution can itself introduce a
            // dangling edge, drop a required property, or create a UNIQUE
            // collision, none of which may be committed. A completing merge
            // always has a branch tip and a base (the branch is frozen at `ours`
            // for the whole merge); their absence means a corrupt or injected
            // MERGE_HEAD, so refuse rather than commit an unrelated history
            // unchecked.
            let tip = parent.ok_or(GraphError::MergeState(
                "merge in progress but the branch is unborn",
            ))?;
            let base = repo
                .merge_base(&tip, &theirs)?
                .ok_or_else(|| GraphError::NoMergeBase {
                    ours: tip.to_hex(),
                    theirs: theirs.to_hex(),
                })?;
            let base_manifest = repo.manifest_at_commit(&base)?;
            let violations =
                crate::merge::validate_merged(&repo.store, &base_manifest, &self.manifest)?;
            if !violations.is_empty() {
                return Err(GraphError::MergeState(
                    "cannot commit: the merge leaves graph-level violations \
                     (dangling edge or constraint breach) — repair the graph, then commit",
                ));
            }
            // Clean: drop any advisory graph-violation entries so the completed
            // manifest carries no conflicts map.
            if self.manifest.conflicts.is_some() {
                self.manifest.conflicts = None;
                self.persist_manifest(self.manifest.clone())?;
            }
        } else if self.manifest.conflicts.is_some() {
            // No merge in progress, yet the workspace still holds a conflicts
            // map: a wedged merge-in-progress workspace with no MERGE_HEAD.
            return Err(GraphError::MergeInProgress);
        }

        let manifest_bytes = self.manifest.encode();
        let anchors = manifest_chunk_set(&repo.store, &self.manifest)?;
        let summary = summarise(&repo.store, &self.manifest)?;
        // `ours` (the branch tip) first; a completing merge adds `theirs`.
        let mut parents: Vec<Hash> = parent.into_iter().collect();
        if let Some(theirs) = merge_head {
            parents.push(theirs);
        }

        let mut new_commit = NewCommit::new(&manifest_bytes, &summary, message);
        new_commit.trailers = trailers;
        new_commit.parents = &parents;
        new_commit.anchors = &anchors;
        if let Some(author) = author {
            new_commit.author = author;
        }
        let commit_id = repo.store.create_commit(&new_commit)?;

        match repo.store.write_ref(&branch, parents.first(), &commit_id) {
            Ok(()) => {
                // The merge is complete: clear MERGE_HEAD so the next commit
                // is an ordinary single-parent one.
                if merge_head.is_some() {
                    repo.store.delete_ref(WORKTREE_MERGE_HEAD_REF)?;
                }
                Ok(commit_id)
            }
            Err(StoreError::CasFailed { .. }) => Err(GraphError::BranchConflict {
                name: branch.clone(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve every cell conflict by taking its value from `source` (the
    /// `ours` or `theirs` manifest), maintaining `edges_rev`, and clear the
    /// merge-in-progress `conflicts` map. Returns the number resolved. Graph
    /// violations cannot be picked a side and must be resolved by ordinary
    /// writes (acetone-14c.4c), so their presence is an error here.
    fn resolve_conflicts_from(&mut self, source: &Manifest) -> Result<usize, GraphError> {
        let params = self.manifest.chunk_params;
        let conflicts_root = self
            .manifest
            .conflicts
            .ok_or(GraphError::MergeState("no conflicts to resolve"))?
            .to_root(params)?;
        let conflicts = crate::conflicts::read_conflicts(&self.repo.store, &conflicts_root)?;
        if conflicts
            .iter()
            .any(|c| !matches!(c, crate::conflicts::PersistedConflict::Cell { .. }))
        {
            return Err(GraphError::MergeState(
                "graph-level violations must be resolved by editing the graph, not by picking a side",
            ));
        }

        // Group the per-property conflicts by their `(map, key)`, so a node or
        // edge conflicted on several properties (ADR-0035, cell-wise merge) is
        // rebuilt once. A whole-record conflict (`property == None`) resolves by
        // taking the entire record from `source`; a set of property conflicts
        // resolves by overwriting only those properties on the merged record,
        // preserving the properties that auto-merged.
        let mut by_key: BTreeMap<(ConflictMap, Vec<u8>), Vec<Option<String>>> = BTreeMap::new();
        for conflict in &conflicts {
            let crate::conflicts::PersistedConflict::Cell { map, key, property } = conflict else {
                continue;
            };
            by_key
                .entry((*map, key.clone()))
                .or_default()
                .push(property.clone());
        }

        let mut count = 0;
        for ((map, key), properties) in &by_key {
            count += properties.len();
            let whole_record = properties.iter().any(Option::is_none);
            let source_root = match map {
                ConflictMap::Schema => source.schema,
                ConflictMap::Nodes => source.nodes,
                ConflictMap::Edges => source.edges_fwd,
            }
            .to_root(params)?;

            if whole_record {
                // Existence is disputed (or a schema key): take the whole record
                // from the chosen side — a `Put` if it has one, else a `Delete`.
                let value = acetone_prolly::get(&self.repo.store, &source_root, key)?;
                self.resolve_whole_record(*map, key, value.as_deref())?;
            } else {
                // Overwrite only the conflicted properties on the merged record,
                // taking each value from the chosen side (or dropping it when
                // that side has none — a resolved delete-vs-modify).
                let names: Vec<&str> = properties.iter().filter_map(|p| p.as_deref()).collect();
                self.resolve_properties(*map, key, &source_root, &names)?;
            }
        }
        // The merge is fully resolved: drop the conflicts map. `save_in_place`
        // persists a transaction only when `is_dirty()` (it does not observe a
        // `conflicts` delta on its own), so clearing the map must always
        // co-occur with a staged map op — which it does: every resolved key
        // stages at least one op, so a non-empty conflicts map yields count > 0
        // and a dirty transaction. Guard the invariant a future refactor could
        // break.
        debug_assert!(
            count == 0 || self.is_dirty(),
            "resolving conflicts must stage a write so `conflicts = None` persists"
        );
        self.manifest.conflicts = None;
        Ok(count)
    }

    /// Resolve a whole-record cell conflict by taking `value` (the chosen
    /// side's encoded record, or `None` to delete) verbatim, mirroring
    /// `edges_rev` for an edge.
    fn resolve_whole_record(
        &mut self,
        map: ConflictMap,
        key: &[u8],
        value: Option<&[u8]>,
    ) -> Result<(), GraphError> {
        let op = match value {
            Some(bytes) => BatchOp::Put(key.to_vec(), bytes.to_vec()),
            None => BatchOp::Delete(key.to_vec()),
        };
        match map {
            ConflictMap::Schema => self.schema.push(op),
            ConflictMap::Nodes => self.nodes.push(op),
            ConflictMap::Edges => {
                self.edges_fwd.push(op);
                // `edges_rev` is derived: mirror the forward change.
                let rev = EdgeKey::decode_fwd(key)?.encode_rev()?;
                self.edges_rev.push(match value {
                    Some(_) => BatchOp::Put(rev, Vec::new()),
                    None => BatchOp::Delete(rev),
                });
            }
        }
        Ok(())
    }

    /// Resolve a set of per-property conflicts on one node/edge: start from the
    /// merged record (its auto-merged properties), overwrite each named property
    /// with the chosen side's value (or drop it when that side lacks it), and
    /// write the record back. The map key — and so the edge's endpoints — is
    /// unchanged, so `edges_rev` needs no update.
    fn resolve_properties(
        &mut self,
        map: ConflictMap,
        key: &[u8],
        source_root: &Root,
        names: &[&str],
    ) -> Result<(), GraphError> {
        let store = &self.repo.store;
        let source_value = acetone_prolly::get(store, source_root, key)?;
        match map {
            ConflictMap::Nodes => {
                let merged_root = self.manifest.nodes.to_root(self.manifest.chunk_params)?;
                let merged = match acetone_prolly::get(store, &merged_root, key)? {
                    Some(bytes) => NodeRecord::decode(&bytes)?,
                    None => NodeRecord::new(Vec::new(), BTreeMap::new()),
                };
                let source = match &source_value {
                    Some(bytes) => Some(NodeRecord::decode(bytes)?),
                    None => None,
                };
                let labels: Vec<String> = merged.secondary_labels().to_vec();
                let mut properties: BTreeMap<String, Value> = merged.properties().clone();
                for name in names {
                    match source.as_ref().and_then(|s| s.properties().get(*name)) {
                        Some(v) => {
                            properties.insert((*name).to_owned(), v.clone());
                        }
                        None => {
                            properties.remove(*name);
                        }
                    }
                }
                let resolved = NodeRecord::new(labels, properties);
                self.nodes
                    .push(BatchOp::Put(key.to_vec(), resolved.encode()?));
            }
            ConflictMap::Edges => {
                let merged_root = self
                    .manifest
                    .edges_fwd
                    .to_root(self.manifest.chunk_params)?;
                let merged = match acetone_prolly::get(store, &merged_root, key)? {
                    Some(bytes) => EdgeRecord::decode(&bytes)?,
                    None => EdgeRecord::new(BTreeMap::new()),
                };
                let source = match &source_value {
                    Some(bytes) => Some(EdgeRecord::decode(bytes)?),
                    None => None,
                };
                let mut properties: BTreeMap<String, Value> = merged.properties().clone();
                for name in names {
                    match source.as_ref().and_then(|s| s.properties().get(*name)) {
                        Some(v) => {
                            properties.insert((*name).to_owned(), v.clone());
                        }
                        None => {
                            properties.remove(*name);
                        }
                    }
                }
                let merged = EdgeRecord::new(properties);
                self.edges_fwd
                    .push(BatchOp::Put(key.to_vec(), merged.encode()?));
            }
            // A schema conflict is never per-property (it has `property == None`
            // and is handled as a whole record).
            ConflictMap::Schema => {
                return Err(GraphError::MergeState(
                    "schema conflicts have no per-property form",
                ));
            }
        }
        Ok(())
    }
}

/// The complete chunk set of a manifest: every chunk of every map root,
/// as sorted anchor list for [`NewCommit::anchors`].
pub(crate) fn manifest_chunk_set(
    store: &GitStore,
    manifest: &Manifest,
) -> Result<Vec<Hash>, GraphError> {
    let params = manifest.chunk_params;
    let mut set = BTreeSet::new();
    let mut add = |map_root: &MapRoot| -> Result<(), GraphError> {
        let root = map_root.to_root(params)?;
        collect_reachable_chunks(store, &root, &mut set)?;
        Ok(())
    };
    add(&manifest.schema)?;
    add(&manifest.nodes)?;
    add(&manifest.edges_fwd)?;
    add(&manifest.edges_rev)?;
    for map_root in manifest.indexes.values() {
        add(map_root)?;
    }
    if let Some(conflicts) = &manifest.conflicts {
        add(conflicts)?;
    }
    Ok(set.into_iter().collect())
}

/// The human-readable summary stored as `README.md` in the commit tree.
pub(crate) fn summarise(store: &GitStore, manifest: &Manifest) -> Result<String, GraphError> {
    let snapshot = Snapshot {
        store,
        manifest: manifest.clone(),
    };
    let nodes = snapshot.count(&manifest.nodes)?;
    let edges = snapshot.count(&manifest.edges_fwd)?;
    let labels = snapshot.schema_entries()?.len();
    Ok(format!(
        "# acetone graph\n\nThis commit is an [acetone](https://github.com/curvelogic/acetone) \
         graph version: {nodes} node(s), {edges} relationship(s), {labels} schema entr(y/ies). \
         The `manifest` file is the machine-readable version record; `chunks/` anchors the \
         version's data.\n"
    ))
}

/// A read-only view of one graph version, pinned to its manifest.
/// Snapshots never observe later writes (MVCC, spec §4).
pub struct Snapshot<'s> {
    store: &'s GitStore,
    manifest: Manifest,
}

impl<'s> Snapshot<'s> {
    /// Pin a read-only view to an arbitrary manifest and store. Used by
    /// [`crate::fsck`], which verifies historical and workspace manifests
    /// that are not the current workspace snapshot.
    pub(crate) fn new(store: &'s GitStore, manifest: Manifest) -> Self {
        Snapshot { store, manifest }
    }

    /// The manifest this snapshot is pinned to.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn root(&self, map_root: &MapRoot) -> Result<Root, GraphError> {
        Ok(map_root.to_root(self.manifest.chunk_params)?)
    }

    fn count(&self, map_root: &MapRoot) -> Result<usize, GraphError> {
        let root = self.root(map_root)?;
        let mut n = 0usize;
        for item in acetone_prolly::scan(self.store, &root, ..)? {
            item?;
            n += 1;
        }
        Ok(n)
    }

    /// Point lookup of a node.
    pub fn get_node(&self, key: &NodeKey) -> Result<Option<NodeRecord>, GraphError> {
        let root = self.root(&self.manifest.nodes)?;
        match acetone_prolly::get(self.store, &root, &key.encode()?)? {
            None => Ok(None),
            Some(bytes) => Ok(Some(NodeRecord::decode(&bytes)?)),
        }
    }

    /// All nodes, in key order.
    pub fn nodes(&self) -> Result<Vec<(NodeKey, NodeRecord)>, GraphError> {
        let root = self.root(&self.manifest.nodes)?;
        let mut out = Vec::new();
        for item in acetone_prolly::scan(self.store, &root, ..)? {
            let (key, value) = item?;
            out.push((NodeKey::decode(&key)?, NodeRecord::decode(&value)?));
        }
        Ok(out)
    }

    /// All edges (from the forward map), in key order.
    pub fn edges(&self) -> Result<Vec<(EdgeKey, EdgeRecord)>, GraphError> {
        let root = self.root(&self.manifest.edges_fwd)?;
        let mut out = Vec::new();
        for item in acetone_prolly::scan(self.store, &root, ..)? {
            let (key, value) = item?;
            out.push((EdgeKey::decode_fwd(&key)?, EdgeRecord::decode(&value)?));
        }
        Ok(out)
    }

    /// The entries of a declared index map `idx/<name>`, in key order, or an
    /// empty vec when no such index is present.
    pub fn index_entries(
        &self,
        name: &str,
    ) -> Result<Vec<acetone_model::graph_keys::IndexEntry>, GraphError> {
        let Some(map_root) = self.manifest.indexes.get(name) else {
            return Ok(Vec::new());
        };
        let root = self.root(map_root)?;
        let mut out = Vec::new();
        for item in acetone_prolly::scan(self.store, &root, ..)? {
            let (key, _) = item?;
            out.push(acetone_model::graph_keys::IndexEntry::decode(&key)?);
        }
        Ok(out)
    }

    /// The node keys a declared index `name` selects for `prefix` — the
    /// memcomparable [`index_value_prefix`](acetone_model::graph_keys::index_value_prefix)
    /// of an equality probe. Read **lazily**: only the index leaves covering the
    /// prefix are loaded, not the whole map. `None` when the index map is absent
    /// (the caller falls back to a scan). The result is a candidate set (in index
    /// order) the caller still filters, so being over-broad is safe; the scan
    /// stops at the first key past the prefix (index order guarantees no later
    /// key matches). This is the store-backed `IndexSeek` primitive (ADR-0040).
    pub fn index_scan(
        &self,
        name: &str,
        prefix: &[u8],
    ) -> Result<Option<Vec<NodeKey>>, GraphError> {
        let Some(map_root) = self.manifest.indexes.get(name) else {
            return Ok(None);
        };
        let root = self.root(map_root)?;
        let mut out = Vec::new();
        for item in acetone_prolly::scan(
            self.store,
            &root,
            (Bound::Included(prefix), Bound::Unbounded),
        )? {
            let (key, _) = item?;
            if !key.starts_with(prefix) {
                break;
            }
            out.push(
                acetone_model::graph_keys::IndexEntry::decode(&key)?
                    .node()
                    .clone(),
            );
        }
        Ok(Some(out))
    }

    /// Edges leading out of `node` (`edges_fwd`, keyed by `src`), read lazily by
    /// a degree-bounded prefix scan — only the leaves holding this node's
    /// out-edges are loaded, not the whole edge map.
    pub fn out_edges(&self, node: &NodeKey) -> Result<Vec<(EdgeKey, EdgeRecord)>, GraphError> {
        self.incident(&self.manifest.edges_fwd, node, false)
    }

    /// Edges leading into `node` (`edges_rev`, keyed by `dst`), read lazily by a
    /// degree-bounded prefix scan.
    pub fn in_edges(&self, node: &NodeKey) -> Result<Vec<(EdgeKey, EdgeRecord)>, GraphError> {
        self.incident(&self.manifest.edges_rev, node, true)
    }

    /// A degree-bounded prefix scan of an edge map: the leading endpoint's key
    /// bytes prefix every edge key incident to it (spec §3.3), so `node`'s key
    /// selects exactly its out-edges (`edges_fwd`) or in-edges (`edges_rev`).
    ///
    /// The forward map stores the [`EdgeRecord`]; the reverse map stores only
    /// keys (Invariant #5), so an in-edge's record is a point lookup on
    /// `edges_fwd`.
    fn incident(
        &self,
        map_root: &MapRoot,
        node: &NodeKey,
        reversed: bool,
    ) -> Result<Vec<(EdgeKey, EdgeRecord)>, GraphError> {
        let prefix = acetone_model::graph_keys::edge_endpoint_prefix(node)?;
        let root = self.root(map_root)?;
        let fwd_root = if reversed {
            Some(self.root(&self.manifest.edges_fwd)?)
        } else {
            None
        };
        let mut out = Vec::new();
        for item in acetone_prolly::scan(
            self.store,
            &root,
            (Bound::Included(prefix.as_slice()), Bound::Unbounded),
        )? {
            let (key, value) = item?;
            if !key.starts_with(&prefix) {
                break;
            }
            let (edge, record) = if reversed {
                let edge = EdgeKey::decode_rev(&key)?;
                let bytes = acetone_prolly::get(
                    self.store,
                    fwd_root.as_ref().expect("set when reversed"),
                    &edge.encode_fwd()?,
                )?
                .ok_or_else(|| GraphError::InconsistentReverseEdge {
                    edge: acetone_model::display::format_node_identity(
                        edge.src().label(),
                        edge.src().key(),
                    ),
                })?;
                (edge, EdgeRecord::decode(&bytes)?)
            } else {
                (EdgeKey::decode_fwd(&key)?, EdgeRecord::decode(&value)?)
            };
            out.push((edge, record));
        }
        Ok(out)
    }

    /// The edge keys of the reverse map, in key order (fsck's
    /// edge-symmetry check reads both maps through this pair).
    pub fn reverse_edge_keys(&self) -> Result<Vec<EdgeKey>, GraphError> {
        let root = self.root(&self.manifest.edges_rev)?;
        let mut out = Vec::new();
        for item in acetone_prolly::scan(self.store, &root, ..)? {
            let (key, _) = item?;
            out.push(EdgeKey::decode_rev(&key)?);
        }
        Ok(out)
    }

    /// Classify the graph-level difference from `self` (the `from` version)
    /// to `to`, over the node and forward-edge maps (spec §7). The reverse
    /// edge map is derived from the forward map and is not diffed. Both
    /// snapshots must belong to the same repository (they share its store);
    /// this is documented, not enforced — but chunks are content-addressed, so
    /// diffing across stores fails cleanly with `ChunkNotFound` (a missing
    /// hash), never silent corruption.
    pub fn diff(&self, to: &Snapshot<'_>) -> Result<GraphDiff, GraphError> {
        let mut nodes = Vec::new();
        let (a, b) = (
            self.root(&self.manifest.nodes)?,
            to.root(&to.manifest.nodes)?,
        );
        for entry in acetone_prolly::diff(self.store, &a, &b)? {
            let entry = entry?;
            let before = entry
                .before
                .as_ref()
                .map(|v| NodeRecord::decode(v.as_ref()))
                .transpose()?;
            let after = entry
                .after
                .as_ref()
                .map(|v| NodeRecord::decode(v.as_ref()))
                .transpose()?;
            nodes.push(NodeChange {
                kind: crate::diff::classify(before.is_some(), after.is_some()),
                key: NodeKey::decode(&entry.key)?,
                before,
                after,
            });
        }
        let mut edges = Vec::new();
        let (a, b) = (
            self.root(&self.manifest.edges_fwd)?,
            to.root(&to.manifest.edges_fwd)?,
        );
        for entry in acetone_prolly::diff(self.store, &a, &b)? {
            let entry = entry?;
            let before = entry
                .before
                .as_ref()
                .map(|v| EdgeRecord::decode(v.as_ref()))
                .transpose()?;
            let after = entry
                .after
                .as_ref()
                .map(|v| EdgeRecord::decode(v.as_ref()))
                .transpose()?;
            edges.push(EdgeChange {
                kind: crate::diff::classify(before.is_some(), after.is_some()),
                key: EdgeKey::decode_fwd(&entry.key)?,
                before,
                after,
            });
        }
        Ok(GraphDiff { nodes, edges })
    }

    /// All schema entries, in key order.
    pub fn schema_entries(&self) -> Result<Vec<SchemaEntry>, GraphError> {
        let root = self.root(&self.manifest.schema)?;
        let mut out = Vec::new();
        for item in acetone_prolly::scan(self.store, &root, ..)? {
            let (key, value) = item?;
            out.push(SchemaEntry::decode(&key, &value)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acetone_model::Value;

    fn node(key: &str) -> NodeKey {
        NodeKey::new("Host", vec![Value::String(key.to_owned())]).expect("valid key")
    }

    /// Commit-time chunk-parameter guard (acetone-093, Gate D freeze
    /// audit): a commit whose workspace manifest carries chunk parameters
    /// different from its parent commit's is rejected with the typed
    /// error, before anything advances. The tampering is only possible
    /// from inside the crate (the manifest field is private) — exactly
    /// why the guard is defence in depth, not a reachable user error.
    #[test]
    fn commit_rejects_chunk_params_that_differ_from_the_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo =
            Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");

        // First commit: no parent, so no agreement to check.
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(&node("web1"), &NodeRecord::new([], Default::default()))
            .expect("put");
        tx.commit("base", &[], None).expect("first commit");

        // Second commit with tampered parameters must be rejected.
        let mut tx = repo.begin_write().expect("begin");
        tx.manifest.chunk_params = ChunkParams::new(512, 10, 8192).expect("valid params");
        tx.put_node(&node("web2"), &NodeRecord::new([], Default::default()))
            .expect("put");
        let err = tx
            .commit("tampered", &[], None)
            .expect_err("mismatched chunk params must not commit");
        assert!(
            matches!(err, GraphError::ChunkParamsMismatch { .. }),
            "expected ChunkParamsMismatch, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("min_bytes 512"), "{msg}");
        assert!(msg.contains("spec §3.2"), "{msg}");

        // The branch did not advance and an untampered commit still works.
        let mut tx = repo.begin_write().expect("begin");
        tx.put_node(&node("web2"), &NodeRecord::new([], Default::default()))
            .expect("put");
        tx.commit("clean", &[], None).expect("untampered commit");
    }
}
