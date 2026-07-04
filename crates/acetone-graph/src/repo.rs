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
//! # Uncommitted workspaces and git gc (ADR-0010)
//!
//! The workspace ref keeps the manifest *blob* alive, but git cannot
//! parse manifests, so the chunks a workspace manifest references are
//! **not** reachable from it. An uncommitted workspace therefore does
//! not survive an aggressive `git gc` by a foreign tool. `acetone gc`
//! must (and later phases do) protect workspace chunk sets; until then:
//! commit before running any external gc.

use crate::error::GraphError;
use crate::lock::WriteLock;
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

/// Namespace of workspace refs.
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
        let manifest_hash = store.put(&manifest.encode())?;
        store.write_ref(&workspace_ref(DEFAULT_WORKSPACE), None, &manifest_hash)?;
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
        // Fail fast: the workspace ref must exist AND its manifest must
        // decode, so a damaged repository is reported at open, not on
        // first use.
        repo.workspace_manifest()?;
        Ok(repo)
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

    fn workspace_manifest_hash(&self) -> Result<Hash, GraphError> {
        self.store
            .read_ref(&workspace_ref(&self.workspace))?
            .ok_or_else(|| GraphError::NoWorkspace {
                name: self.workspace.clone(),
            })
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
        let _lock = WriteLock::acquire(self.store.common_dir())?;
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
        let old = self.workspace_manifest_hash()?;
        if old != manifest_hash {
            self.cas_workspace(&old, &manifest_hash)?;
        }
        self.store.set_head(&full)?;
        Ok(())
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
        let lock = WriteLock::acquire(self.store.common_dir())?;
        let base_hash = self.workspace_manifest_hash()?;
        let manifest = read_manifest_chunk(&self.store, &base_hash)?;
        Ok(Transaction {
            repo: self,
            _lock: lock,
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

    fn cas_workspace(&self, old: &Hash, new: &Hash) -> Result<(), GraphError> {
        match self
            .store
            .write_ref(&workspace_ref(&self.workspace), Some(old), new)
        {
            Ok(()) => Ok(()),
            Err(StoreError::CasFailed { .. }) => Err(GraphError::WorkspaceConflict {
                name: self.workspace.clone(),
            }),
            Err(e) => Err(e.into()),
        }
    }
}

fn workspace_ref(name: &str) -> String {
    format!("{WORKSPACE_REF_PREFIX}{name}")
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
            conflicts: self.manifest.conflicts,
        };
        let new_hash = store.put(&manifest.encode())?;
        if new_hash != self.base_hash {
            self.repo.cas_workspace(&self.base_hash, &new_hash)?;
        }
        self.base_hash = new_hash;
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
        if self.manifest.conflicts.is_some() {
            return Err(GraphError::MergeInProgress);
        }
        self.save_in_place()?;

        let repo = self.repo;
        let branch = repo.current_branch()?.ok_or(GraphError::NoCurrentBranch)?;
        let parent = repo.store.read_ref(&branch)?;

        let manifest_bytes = self.manifest.encode();
        let anchors = manifest_chunk_set(&repo.store, &self.manifest)?;
        let summary = summarise(&repo.store, &self.manifest)?;
        let parents: Vec<Hash> = parent.into_iter().collect();

        let mut new_commit = NewCommit::new(&manifest_bytes, &summary, message);
        new_commit.trailers = trailers;
        new_commit.parents = &parents;
        new_commit.anchors = &anchors;
        if let Some(author) = author {
            new_commit.author = author;
        }
        let commit_id = repo.store.create_commit(&new_commit)?;

        match repo.store.write_ref(&branch, parents.first(), &commit_id) {
            Ok(()) => Ok(commit_id),
            Err(StoreError::CasFailed { .. }) => Err(GraphError::BranchConflict {
                name: branch.clone(),
            }),
            Err(e) => Err(e.into()),
        }
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
