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
use crate::merge::{ConflictMap, ManifestMerge, MergeConflict, MergeOutcome, merge_manifests};
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::{Manifest, MapRoot};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::SchemaEntry;
use acetone_prolly::{BatchOp, ChunkParams, Root, collect_reachable_chunks};
use acetone_store::{
    ChunkStore, CommitStore, GitStore, GitStoreOptions, Hash, NewCommit, ObjectFormat, RefStore,
    Signature, StoreError,
};
use std::collections::BTreeSet;
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
/// Namespace of branches (git-native).
pub const BRANCH_REF_PREFIX: &str = "refs/heads/";
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

        let empty = acetone_prolly::empty(&store, options.chunk_params)?;
        let manifest = Manifest {
            chunk_params: options.chunk_params,
            schema: MapRoot::from_root(&empty),
            nodes: MapRoot::from_root(&empty),
            edges_fwd: MapRoot::from_root(&empty),
            edges_rev: MapRoot::from_root(&empty),
            indexes: Default::default(),
            conflicts: None,
        };
        // The workspace ref points at a workspace tree that anchors the
        // manifest's chunk set, so uncommitted state survives a foreign gc
        // (huo). For the empty graph that is just the empty prolly root.
        let anchors = manifest_chunk_set(&store, &manifest)?;
        let tree = store.write_workspace_tree(&manifest.encode(), &anchors)?;
        store.write_ref(WORKTREE_WORKSPACE_REF, None, &tree)?;
        store.set_head(&format!("{BRANCH_REF_PREFIX}{DEFAULT_BRANCH}"))?;
        Ok(Repository {
            store,
            workspace: DEFAULT_WORKSPACE.to_owned(),
        })
    }

    /// Open an existing acetone repository (its default workspace).
    /// Errors with [`GraphError::NoWorkspace`] if the git repository was
    /// never initialised by acetone.
    pub fn open(path: &Path) -> Result<Repository, GraphError> {
        let store = GitStore::open(path)?;
        let repo = Repository {
            store,
            workspace: DEFAULT_WORKSPACE.to_owned(),
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
    /// its per-worktree workspace from the checked-out branch's committed
    /// manifest, so opening a new worktree "just works" (ADR-0014). The
    /// bootstrap takes the writer lock only in that first-time case; an
    /// already-provisioned worktree opens lock-free.
    fn ensure_workspace(&self) -> Result<(), GraphError> {
        if self.workspace_ref_value()?.is_some() || self.legacy_ref_value()?.is_some() {
            return Ok(());
        }
        let _lock = WriteLock::acquire(self.store.git_dir())?;
        // Re-check under the lock: another process may have bootstrapped.
        if self.workspace_ref_value()?.is_some() {
            return Ok(());
        }
        match self.head_commit()? {
            Some(commit) => {
                let manifest_hash = self.commit_manifest_hash(&commit)?;
                let tree = self.workspace_tree_for(&manifest_hash)?;
                self.cas_workspace(None, &tree)
            }
            // No workspace and an unborn branch: nothing to bootstrap from.
            None => Err(GraphError::NoWorkspace {
                name: self.workspace.clone(),
            }),
        }
    }

    /// Build the workspace tree (`{manifest, chunks/}`, huo) for the manifest
    /// blob `manifest_hash`, anchoring its complete chunk set. Returns the
    /// tree hash the workspace ref should point at.
    fn workspace_tree_for(&self, manifest_hash: &Hash) -> Result<Hash, GraphError> {
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

    /// The full ref name of the checked-out branch, if the checked-out
    /// ref is a branch.
    pub fn current_branch(&self) -> Result<Option<String>, GraphError> {
        Ok(self
            .store
            .read_head()?
            .filter(|name| name.starts_with(BRANCH_REF_PREFIX)))
    }

    /// The commit the checked-out branch points at (`None` while the
    /// branch is unborn).
    pub fn head_commit(&self) -> Result<Option<Hash>, GraphError> {
        match self.current_branch()? {
            None => Ok(None),
            Some(branch) => Ok(self.store.read_ref(&branch)?),
        }
    }

    /// Whether the workspace differs from the checked-out branch's
    /// committed state. True when the branch is unborn and the workspace
    /// is no longer the empty graph it was initialised with — and, always,
    /// when the workspace manifest differs from the head commit's.
    pub fn is_dirty(&self) -> Result<bool, GraphError> {
        let workspace = self.workspace_manifest_hash()?;
        match self.head_commit()? {
            None => {
                // Unborn branch: dirty iff the workspace has left its
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
            .list_refs(BRANCH_REF_PREFIX)?
            .into_iter()
            .map(|(name, hash)| {
                let short = name
                    .strip_prefix(BRANCH_REF_PREFIX)
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
        let full = format!("{BRANCH_REF_PREFIX}{name}");
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
        let full = format!("{BRANCH_REF_PREFIX}{name}");
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
        self.store.set_head(&full)?;
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
    /// Preconditions: the workspace has no uncommitted changes
    /// ([`GraphError::DirtyWorkspace`]) and the checked-out ref is a branch
    /// ([`GraphError::NoCurrentBranch`]) — merging advances that branch.
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
        if self.is_dirty()? {
            return Err(GraphError::DirtyWorkspace);
        }
        let branch = self.current_branch()?.ok_or(GraphError::NoCurrentBranch)?;
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
                // Only cell conflicts enter merge-in-progress: they are
                // resolvable (`resolve --all-ours|--all-theirs`) and
                // completable now. Graph-level violations have no resolution
                // or abort verb yet (acetone-14c.4c), so persisting them would
                // wedge the workspace — leave the repository unchanged and just
                // report them, as before this bead. Conflicts are homogeneous
                // (cell XOR graph), so this is an all-or-nothing check.
                if !conflicts
                    .iter()
                    .all(|c| matches!(c, MergeConflict::Cell(_)))
                {
                    return Ok(MergeOutcome::Conflicts(conflicts));
                }
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
                    Ok(()) => Ok(MergeOutcome::Merged(commit)),
                    Err(StoreError::CasFailed { .. }) => Err(GraphError::BranchConflict {
                        name: branch.clone(),
                    }),
                    Err(e) => Err(e.into()),
                }
            }
        }
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
    pub fn blame(&self, key: &NodeKey) -> Result<Vec<Hash>, GraphError> {
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

    /// Resolve a refspec — branch short name, full ref name, or hex
    /// commit hash — to a commit address.
    ///
    /// A refspec that fails one interpretation (not a valid ref name, a
    /// hex address naming a non-commit object) simply falls through to
    /// the next and ultimately to [`GraphError::UnresolvedRefspec`];
    /// genuine store damage still surfaces as its own error.
    pub fn resolve_commit(&self, refspec: &str) -> Result<Hash, GraphError> {
        let as_branch = format!("{BRANCH_REF_PREFIX}{refspec}");
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
    fn commit_manifest_hash(&self, commit: &Hash) -> Result<Hash, GraphError> {
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
    fn merge_base(&self, a: &Hash, b: &Hash) -> Result<Option<Hash>, GraphError> {
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
    fn cas_workspace(&self, expected: Option<&Hash>, new: &Hash) -> Result<(), GraphError> {
        match self.store.write_ref(WORKTREE_WORKSPACE_REF, expected, new) {
            Ok(()) => Ok(()),
            Err(StoreError::CasFailed { .. }) => Err(GraphError::WorkspaceConflict {
                name: self.workspace.clone(),
            }),
            Err(e) => Err(e.into()),
        }
    }
}

/// Render a node key's values for an error message (`[a, 1]`-style).
fn render_node_key(key: &NodeKey) -> String {
    let parts: Vec<String> = key.key().iter().map(|v| format!("{v:?}")).collect();
    format!("[{}]", parts.join(", "))
}

fn workspace_ref(name: &str) -> String {
    format!("{WORKSPACE_REF_PREFIX}{name}")
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

fn read_manifest_chunk(store: &GitStore, hash: &Hash) -> Result<Manifest, GraphError> {
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
        Ok(MapRoot::from_root(&acetone_prolly::apply_batch(
            store, &root, ops,
        )?))
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
        let manifest = Manifest {
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
            indexes: self.manifest.indexes.clone(),
            conflicts,
        };
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
        // A merge in progress (MERGE_HEAD set) completes here: the commit gets
        // `theirs` as a second parent (spec §6). It may only complete once
        // every conflict is resolved. Apply staged writes first, so a write
        // that resolves the last conflict in this same transaction is seen
        // (14c.4c) before the unresolved-conflicts check.
        let merge_head = repo.store.read_ref(WORKTREE_MERGE_HEAD_REF)?;
        self.save_in_place()?;
        if self.manifest.conflicts.is_some() {
            return Err(match merge_head {
                Some(_) => GraphError::MergeState(
                    "cannot commit: unresolved merge conflicts remain — resolve them first",
                ),
                None => GraphError::MergeInProgress,
            });
        }

        let branch = repo.current_branch()?.ok_or(GraphError::NoCurrentBranch)?;
        let parent = repo.store.read_ref(&branch)?;

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

        let mut count = 0;
        for conflict in &conflicts {
            let crate::conflicts::PersistedConflict::Cell { map, key } = conflict else {
                continue;
            };
            let source_root = match map {
                ConflictMap::Schema => source.schema,
                ConflictMap::Nodes => source.nodes,
                ConflictMap::Edges => source.edges_fwd,
            }
            .to_root(params)?;
            let value = acetone_prolly::get(&self.repo.store, &source_root, key)?;
            let op = match &value {
                Some(bytes) => BatchOp::Put(key.clone(), bytes.to_vec()),
                None => BatchOp::Delete(key.clone()),
            };
            match map {
                ConflictMap::Schema => self.schema.push(op),
                ConflictMap::Nodes => self.nodes.push(op),
                ConflictMap::Edges => {
                    self.edges_fwd.push(op);
                    // `edges_rev` is derived: mirror the forward change.
                    let rev = EdgeKey::decode_fwd(key)?.encode_rev()?;
                    self.edges_rev.push(match &value {
                        Some(_) => BatchOp::Put(rev, Vec::new()),
                        None => BatchOp::Delete(rev),
                    });
                }
            }
            count += 1;
        }
        // The merge is fully resolved: drop the conflicts map.
        self.manifest.conflicts = None;
        Ok(count)
    }
}

/// The complete chunk set of a manifest: every chunk of every map root,
/// as sorted anchor list for [`NewCommit::anchors`].
fn manifest_chunk_set(store: &GitStore, manifest: &Manifest) -> Result<Vec<Hash>, GraphError> {
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
fn summarise(store: &GitStore, manifest: &Manifest) -> Result<String, GraphError> {
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
