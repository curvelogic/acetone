//! Graph-level three-way merge (spec §7, shaping Decision 4; Phase 4,
//! acetone-14c.2).
//!
//! [`merge_manifests`] is the pure, deterministic core: it three-way-merges
//! the `schema`, `nodes` and `edges_fwd` maps of two versions against their
//! common base via the prolly three-way merge ([`acetone_prolly::merge`]),
//! whose result depends only on the three maps' contents — Load-Bearing
//! Invariant #4 (merge determinism). Conflicts are **data**, not errors
//! (ADR-0007): a conflicted key is absent from the merged map and reported
//! in the outcome.
//!
//! `edges_rev` is a **derived** map (Invariant #5), so it is not merged
//! independently — it is rebuilt from the merged `edges_fwd`, guaranteeing
//! forward/reverse symmetry no matter how the two sides diverged.
//!
//! The commit-graph wrapper ([`crate::repo::Repository::merge`]) resolves
//! the merge base and turns a clean result into a two-parent merge commit;
//! persisting the conflicts map and `resolve` arrive with acetone-14c.4.

use acetone_model::graph_keys::EdgeKey;
use acetone_model::manifest::{Manifest, MapRoot};
use acetone_prolly::{BatchOp, ChunkParams, Root, apply_batch, empty, merge as prolly_merge, scan};
use acetone_store::ChunkStore;

use crate::error::GraphError;

/// Which graph map a conflict arose in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictMap {
    /// The schema map.
    Schema,
    /// The nodes map.
    Nodes,
    /// The forward edges map.
    Edges,
}

/// One key that changed incompatibly on both sides — a conflict, carried as
/// data. The raw encoded key and the three side values are preserved so the
/// resolution machinery (acetone-14c.4) can render and resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflict {
    /// Which map the key belongs to.
    pub map: ConflictMap,
    /// The conflicted key (encoded), absent from the merged map.
    pub key: Vec<u8>,
    /// The value in the merge base, if present.
    pub base: Option<Vec<u8>>,
    /// The value in `ours`, if present.
    pub ours: Option<Vec<u8>>,
    /// The value in `theirs`, if present.
    pub theirs: Option<Vec<u8>>,
}

/// The outcome of a graph-level three-way merge.
#[derive(Debug)]
pub enum ManifestMerge {
    /// A clean merge: the merged manifest, with `edges_rev` rebuilt from the
    /// merged forward map and no conflicts recorded.
    Clean(Box<Manifest>),
    /// Conflicts in `(map, key)` order; no merged manifest is produced here
    /// (persisting the conflicts map and resolving them is acetone-14c.4).
    Conflicts(Vec<MergeConflict>),
}

/// Three-way merge of graph manifests `ours` and `theirs` against their
/// common `base`. Deterministic and symmetric for a clean merge: the merged
/// roots depend only on the three inputs' contents, not on which side is
/// "ours" (Invariant #4). All three must share the repository's chunk
/// parameters.
pub fn merge_manifests<S: ChunkStore>(
    store: &S,
    base: &Manifest,
    ours: &Manifest,
    theirs: &Manifest,
) -> Result<ManifestMerge, GraphError> {
    let params = base.chunk_params;
    // Chunk parameters are fixed per repository (spec §3.2); all three
    // manifests must agree. `to_root(params)` below stamps base's params
    // onto every side, which would defeat the prolly `ParamsMismatch`
    // guard, so assert the precondition the public API documents.
    debug_assert!(
        ours.chunk_params == params && theirs.chunk_params == params,
        "merge inputs must share chunk parameters (fixed per repository)"
    );
    // Secondary indexes are a derived map; merging them would need a rebuild
    // from the merged nodes. None exist before Phase 5, so rather than
    // silently drop a populated index map, refuse it explicitly.
    if !base.indexes.is_empty() || !ours.indexes.is_empty() || !theirs.indexes.is_empty() {
        return Err(GraphError::MergeUnsupported {
            feature: "secondary indexes (arrive in Phase 5)",
        });
    }
    let mut conflicts = Vec::new();

    let schema = merge_one(
        store,
        ConflictMap::Schema,
        |m| &m.schema,
        base,
        ours,
        theirs,
        &mut conflicts,
    )?;
    let nodes = merge_one(
        store,
        ConflictMap::Nodes,
        |m| &m.nodes,
        base,
        ours,
        theirs,
        &mut conflicts,
    )?;
    let edges_fwd = merge_one(
        store,
        ConflictMap::Edges,
        |m| &m.edges_fwd,
        base,
        ours,
        theirs,
        &mut conflicts,
    )?;

    if !conflicts.is_empty() {
        return Ok(ManifestMerge::Conflicts(conflicts));
    }

    // `edges_rev` is derived: rebuild it from the merged forward map rather
    // than merging it, so forward and reverse can never diverge (Invariant
    // #5). Secondary `indexes` are likewise derived; there are none before
    // Phase 5, and they are rebuilt when they arrive.
    let edges_rev = rebuild_reverse(store, &edges_fwd, params)?;

    Ok(ManifestMerge::Clean(Box::new(Manifest {
        chunk_params: params,
        schema: MapRoot::from_root(&schema),
        nodes: MapRoot::from_root(&nodes),
        edges_fwd: MapRoot::from_root(&edges_fwd),
        edges_rev: MapRoot::from_root(&edges_rev),
        indexes: Default::default(),
        conflicts: None,
    })))
}

/// Three-way merge one map, appending any conflicts (tagged with `map`).
fn merge_one<S: ChunkStore>(
    store: &S,
    map: ConflictMap,
    select: fn(&Manifest) -> &MapRoot,
    base: &Manifest,
    ours: &Manifest,
    theirs: &Manifest,
    conflicts: &mut Vec<MergeConflict>,
) -> Result<Root, GraphError> {
    let params = base.chunk_params;
    let outcome = prolly_merge(
        store,
        &select(base).to_root(params)?,
        &select(ours).to_root(params)?,
        &select(theirs).to_root(params)?,
    )?;
    for c in outcome.conflicts {
        conflicts.push(MergeConflict {
            map,
            key: c.key.to_vec(),
            base: c.base.map(|b| b.to_vec()),
            ours: c.ours.map(|b| b.to_vec()),
            theirs: c.theirs.map(|b| b.to_vec()),
        });
    }
    Ok(outcome.root)
}

/// Rebuild the reverse edge map from a forward edge map: one key-only entry
/// per forward edge, re-encoded in reverse order. The reverse map mirrors
/// the forward map exactly (Invariant #5; the same relation `fsck` checks).
fn rebuild_reverse<S: ChunkStore>(
    store: &S,
    edges_fwd: &Root,
    params: ChunkParams,
) -> Result<Root, GraphError> {
    let mut ops = Vec::new();
    for item in scan(store, edges_fwd, ..)? {
        let (key, _) = item?;
        ops.push(BatchOp::Put(
            EdgeKey::decode_fwd(&key)?.encode_rev()?,
            Vec::new(),
        ));
    }
    let base = empty(store, params)?;
    Ok(apply_batch(store, &base, ops)?)
}
