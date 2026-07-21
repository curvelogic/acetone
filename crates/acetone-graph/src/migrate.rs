//! History rewrite (`acetone migrate`): re-encode every reachable graph
//! version under a format transform and rebuild the commit graph, producing
//! new hashes.
//!
//! This is the Gate D (ADR-0024) escape hatch that makes the format freeze
//! safe: when a future `format_version` bump changes an encoding, `migrate`
//! walks all history, re-encodes each version, and rewrites the commit graph
//! so a repository can be brought forward (new hashes are expected and
//! accepted pre-1.0). See ADR-0025.
//!
//! The engine here is generic over a [`FormatTransform`]. Per the Gate D
//! decision ("engine now, demo at 0.2"), the only transform shipped today is
//! [`Rechunk`] — a **version-preserving** rebuild under new chunk parameters
//! (chunk parameters are manifest data, so the encoding and `format_version`
//! are unchanged) that nonetheless rewrites every root and commit hash, which
//! is exactly what exercises the engine end to end. A real cross-version
//! transform slots into the same engine at the first 0.2 format change.
//!
//! **Fidelity.** Each commit is rebuilt with its message, author and committer
//! — identity *and* timestamp — preserved verbatim ([`GitStore::rewrite_commit`]),
//! parents remapped to the rewritten commits, and a fresh anchor set for the
//! transformed manifest. `git fsck` stays clean.
//!
//! **Scope / limitations (first cut).** Rewrites `refs/heads/*` and
//! `refs/tags/*` whose targets are commits; an annotated-tag object (a tag
//! that is not a direct commit) is reported as [`GraphError::NotACommit`]
//! rather than silently skipped, and a checked-out **detached HEAD** (not
//! under `refs/heads/`) is left pointing at its now-superseded commit — the
//! CLI does not expose detached HEAD. Requires a clean, non-merging workspace,
//! which it resets to the rewritten head. Ref swings are CAS, one at a time
//! (safe under acetone's single-writer model), not a single atomic
//! transaction: a **completed** migration is deterministic and idempotent
//! (re-running produces the same hashes and is a no-op), but a process that
//! dies mid-migration — after some refs are swung but before the workspace is
//! reset — leaves the workspace lagging the branch, which reads as *dirty* and
//! makes a bare re-run refuse. No data is lost (refs point at the correct
//! rewritten commits); recover with `acetone checkout <branch>` to resync the
//! workspace. A single atomic swing (or a journaled, resumable migration) is a
//! tracked follow-up.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use acetone_model::manifest::{Manifest, MapRoot};
use acetone_prolly::{ChunkParams, bulk_load, scan};
use acetone_store::{CommitStore, GitStore, Hash, RefStore, RewriteCommit};

use crate::error::GraphError;
use crate::repo::{Repository, WORKTREE_WORKSPACE_REF, manifest_chunk_set, summarise};

/// A transform applied to every graph version during a migration: it maps one
/// version's manifest to a new manifest, writing any new chunks to `store`. It
/// MUST be a pure function of the input version so the migration is
/// deterministic (identical repository + transform ⇒ identical new hashes).
pub trait FormatTransform {
    /// Transform `old` into the new manifest, writing new chunks to `store`.
    fn transform(&self, store: &GitStore, old: &Manifest) -> Result<Manifest, GraphError>;
}

/// Rebuild every map under new chunk parameters. Version-preserving — the key
/// and value encodings are unchanged and `format_version` stays the same,
/// because chunk parameters are manifest data — yet it rewrites every prolly
/// root (spec §3.2: changing the chunking changes every hash). A real
/// operation (retuning chunk size) and the vehicle that exercises the engine.
pub struct Rechunk {
    params: ChunkParams,
}

impl Rechunk {
    /// Rebuild every map under `params`.
    pub fn new(params: ChunkParams) -> Self {
        Rechunk { params }
    }

    /// Build a re-chunk transform from raw chunk parameters, validating them —
    /// so callers (e.g. the CLI) need not depend on `acetone-prolly` directly.
    pub fn from_raw(min_bytes: u32, mask_bits: u32, max_bytes: u32) -> Result<Self, GraphError> {
        Ok(Rechunk::new(ChunkParams::new(
            min_bytes, mask_bits, max_bytes,
        )?))
    }

    /// Read a map under its current parameters and rebuild it under the target
    /// parameters, returning the new root. Contents are unchanged, so the
    /// rebuild is history-independent and deterministic.
    fn rechunk_map(
        &self,
        store: &GitStore,
        old_params: ChunkParams,
        map_root: &MapRoot,
    ) -> Result<MapRoot, GraphError> {
        let root = map_root.to_root(old_params)?;
        let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for item in scan(store, &root, ..)? {
            let (key, value) = item?;
            entries.push((key.to_vec(), value.to_vec()));
        }
        let new_root = bulk_load(store, self.params, entries)?;
        Ok(MapRoot::from_root(&new_root))
    }
}

impl FormatTransform for Rechunk {
    fn transform(&self, store: &GitStore, old: &Manifest) -> Result<Manifest, GraphError> {
        let mut indexes = BTreeMap::new();
        for (name, map_root) in &old.indexes {
            indexes.insert(
                name.clone(),
                self.rechunk_map(store, old.chunk_params, map_root)?,
            );
        }
        let conflicts = match &old.conflicts {
            Some(c) => Some(self.rechunk_map(store, old.chunk_params, c)?),
            None => None,
        };
        Ok(Manifest {
            chunk_params: self.params,
            schema: self.rechunk_map(store, old.chunk_params, &old.schema)?,
            nodes: self.rechunk_map(store, old.chunk_params, &old.nodes)?,
            edges_fwd: self.rechunk_map(store, old.chunk_params, &old.edges_fwd)?,
            edges_rev: self.rechunk_map(store, old.chunk_params, &old.edges_rev)?,
            indexes,
            conflicts,
        })
    }
}

/// What a migration rewrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrateReport {
    /// Number of commits rewritten.
    pub commits_rewritten: usize,
    /// Number of refs (branches + tags) repointed.
    pub refs_updated: usize,
}

/// Rewrite all history reachable from `refs/heads/*` and `refs/tags/*` under
/// `transform`, producing new hashes, and repoint every such ref and the
/// default workspace at the rewritten commits.
///
/// Requires a clean, non-merging workspace (which it resets to the rewritten
/// head). Deterministic: the same repository and transform always yield the
/// same new hashes, so re-running is idempotent. Not a single atomic
/// transaction across refs — it CAS-swings each ref in turn, which is safe
/// under acetone's single-writer model.
pub fn rewrite_history(
    repo: &Repository,
    transform: &dyn FormatTransform,
) -> Result<MigrateReport, GraphError> {
    // The rewrite resets the workspace, so refuse to run over uncommitted or
    // mid-merge state.
    if repo.workspace_manifest()?.conflicts.is_some() {
        return Err(GraphError::MergeInProgress);
    }
    if repo.is_dirty()? {
        return Err(GraphError::DirtyWorkspace);
    }

    let store = repo.store();
    let namespace = repo.namespace();
    let mut refs: Vec<(String, Hash)> = Vec::new();
    refs.extend(store.list_refs(namespace.branch_prefix())?);
    refs.extend(store.list_refs(namespace.tag_prefix())?);

    let order = topo_order(store, &refs)?;

    // Rewrite each commit, parents-first, remapping parents to their rewrites.
    let mut mapping: HashMap<Hash, Hash> = HashMap::new();
    for old_id in &order {
        let commit = store.read_commit(old_id)?.ok_or_else(|| {
            GraphError::Migrate(format!("reachable commit {old_id} vanished mid-rewrite"))
        })?;
        let old_manifest = Manifest::decode(&commit.manifest)?;
        let new_manifest = transform.transform(store, &old_manifest)?;
        let manifest_bytes = new_manifest.encode();
        let anchors = manifest_chunk_set(store, &new_manifest)?;
        let summary = summarise(store, &new_manifest)?;
        let new_parents: Vec<Hash> = commit
            .parents
            .iter()
            .map(|p| {
                mapping.get(p).copied().ok_or_else(|| {
                    GraphError::Migrate(format!("parent {p} was not rewritten before its child"))
                })
            })
            .collect::<Result<_, _>>()?;
        let mut spec = RewriteCommit::new(
            &manifest_bytes,
            &summary,
            &commit.message,
            &commit.author,
            &commit.committer,
        );
        spec.parents = &new_parents;
        spec.anchors = &anchors;
        let new_id = store.rewrite_commit(&spec)?;
        mapping.insert(*old_id, new_id);
    }

    // Swing every ref to its rewritten commit (CAS old → new).
    for (name, old_target) in &refs {
        let new_target = mapping.get(old_target).ok_or_else(|| {
            GraphError::Migrate(format!(
                "ref {name:?} target {old_target} was not rewritten"
            ))
        })?;
        store.write_ref(name, Some(old_target), new_target)?;
    }

    // Resync the default workspace to the rewritten head so the repository is
    // consistent (the workspace was clean, i.e. equal to the old head).
    if let Some(new_head) = repo.head_commit()? {
        let manifest_hash = repo.commit_manifest_hash(&new_head)?;
        let tree = repo.workspace_tree_for(&manifest_hash)?;
        let current = store.read_ref(WORKTREE_WORKSPACE_REF)?;
        store.write_ref(WORKTREE_WORKSPACE_REF, current.as_ref(), &tree)?;
    }

    Ok(MigrateReport {
        commits_rewritten: order.len(),
        refs_updated: refs.len(),
    })
}

/// Collect every commit reachable from the ref targets and return them in a
/// topological order with parents before children. A ref target that is not a
/// commit (e.g. an annotated-tag object) is a [`GraphError::NotACommit`].
fn topo_order(store: &GitStore, refs: &[(String, Hash)]) -> Result<Vec<Hash>, GraphError> {
    // Reachable set with each commit's parents.
    let mut parents_of: HashMap<Hash, Vec<Hash>> = HashMap::new();
    let mut stack: Vec<Hash> = Vec::new();
    for (name, target) in refs {
        // Anything that is not readable as a commit here — an annotated-tag
        // object, a blob, or a genuinely damaged object — aborts the whole
        // migration before any object is rewritten or any ref is swung. The
        // common case is an annotated tag, so `NotACommit` names it; a rarer
        // read failure is reported the same way rather than misread.
        if !matches!(store.read_commit(target), Ok(Some(_))) {
            return Err(GraphError::NotACommit { name: name.clone() });
        }
        stack.push(*target);
    }
    while let Some(h) = stack.pop() {
        if parents_of.contains_key(&h) {
            continue;
        }
        let commit = store
            .read_commit(&h)?
            .ok_or_else(|| GraphError::Migrate(format!("reachable commit {h} is absent")))?;
        for parent in &commit.parents {
            stack.push(*parent);
        }
        parents_of.insert(h, commit.parents);
    }

    // Kahn's algorithm; a sorted ready-set makes the order deterministic.
    let mut children: HashMap<Hash, Vec<Hash>> = HashMap::new();
    let mut indegree: HashMap<Hash, usize> = HashMap::new();
    for (h, parents) in &parents_of {
        indegree.insert(*h, parents.len());
        for parent in parents {
            children.entry(*parent).or_default().push(*h);
        }
    }
    let mut ready: BTreeSet<Hash> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(h, _)| *h)
        .collect();
    let mut order = Vec::with_capacity(parents_of.len());
    while let Some(h) = ready.iter().next().copied() {
        ready.remove(&h);
        order.push(h);
        if let Some(cs) = children.get(&h) {
            for c in cs {
                let d = indegree
                    .get_mut(c)
                    .expect("every child has an indegree entry");
                *d -= 1;
                if *d == 0 {
                    ready.insert(*c);
                }
            }
        }
    }
    if order.len() != parents_of.len() {
        return Err(GraphError::Migrate(
            "commit graph has a cycle (impossible for git history)".into(),
        ));
    }
    Ok(order)
}
