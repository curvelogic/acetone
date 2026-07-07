//! Graph-level three-way merge (spec §7, shaping Decision 4; Phase 4,
//! acetone-14c.2, acetone-14c.3).
//!
//! [`merge_manifests`] is the pure, deterministic core. It three-way-merges
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
//! A map-clean merge is then **graph-validated** (acetone-14c.3, ADR-0016):
//! independently merging each map can still produce a referentially- or
//! constraint-invalid graph — e.g. one side adds an edge while the other
//! deletes its endpoint (dangling edge), or two sides add nodes that collide
//! on a UNIQUE property. [`validate_merged`] re-checks referential integrity
//! and schema constraints over the keys the merge changed and surfaces any
//! breach as a [`GraphViolation`] conflict. Validation is a pure function of
//! the base and merged manifests, so it stays deterministic.
//!
//! The commit-graph wrapper ([`crate::repo::Repository::merge`]) resolves
//! the merge base and turns a clean result into a two-parent merge commit;
//! persisting the conflicts map and `resolve` arrive with acetone-14c.4.

use std::collections::{BTreeMap, BTreeSet};

use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::manifest::{Manifest, MapRoot};
use acetone_model::records::{NodeRecord, RecordEncodeError};
use acetone_model::schema::{LabelDef, SchemaEntry};
use acetone_model::values::encode_value;
use acetone_prolly::{
    BatchOp, ChunkParams, Root, apply_batch, diff as prolly_diff, empty, get,
    merge as prolly_merge, scan,
};
use acetone_store::{ChunkStore, Hash};

use crate::error::GraphError;

/// The outcome of the commit-graph merge wrapper
/// ([`crate::repo::Repository::merge`]) — the four ways merging one version
/// into the current branch can resolve.
#[derive(Debug)]
pub enum MergeOutcome {
    /// The version to merge was already an ancestor of the current branch
    /// (including equal): nothing changed.
    AlreadyUpToDate,
    /// The current branch was an ancestor of the version to merge, so it
    /// fast-forwarded — no merge commit was created. Carries the new head.
    FastForward(Hash),
    /// A genuine three-way merge that resolved cleanly: a two-parent merge
    /// commit was written and the branch advanced to it. Carries the merge
    /// commit's address.
    Merged(Hash),
    /// The merge conflicted; no commit was written. For **cell** conflicts the
    /// workspace enters merge-in-progress (the conflicts are persisted and
    /// MERGE_HEAD is set; resolve and complete with `resolve`/`commit`,
    /// acetone-14c.4a). For **graph-level** violations, which have no
    /// resolution verb yet, the repository is left unchanged and the
    /// violations are only reported (acetone-14c.4c). Carries the conflicts in
    /// category-then-key order.
    Conflicts(Vec<MergeConflict>),
}

/// Which graph map a cell-level conflict arose in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictMap {
    /// The schema map.
    Schema,
    /// The nodes map.
    Nodes,
    /// The forward edges map.
    Edges,
}

/// One key that changed incompatibly on both sides — a cell-level conflict.
/// The raw encoded key and the three side values are preserved so the
/// resolution machinery (acetone-14c.4) can render and resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellConflict {
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

/// Which endpoint of a forward edge a [`GraphViolation::DanglingEdge`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// The source (tail) node.
    Src,
    /// The destination (head) node.
    Dst,
}

/// A graph-level violation introduced by an otherwise map-clean merge —
/// broken referential integrity or a breached schema constraint (spec §7,
/// acetone-14c.3). Surfaced as a conflict (data), never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphViolation {
    /// A forward edge whose `endpoint` node is absent from the merged graph:
    /// one side kept or added the edge while the other deleted the endpoint.
    DanglingEdge {
        /// The dangling forward edge key (encoded).
        edge: Vec<u8>,
        /// The absent endpoint's node key (encoded).
        endpoint: Vec<u8>,
        /// Which end of the edge is absent.
        role: Endpoint,
    },
    /// A merged node lacks a property its primary label requires (an
    /// existence constraint, spec §2).
    MissingRequired {
        /// The offending node's key (encoded).
        node: Vec<u8>,
        /// The required property that is absent.
        property: String,
    },
    /// Two or more merged nodes of `label` share a value for a UNIQUE
    /// property (spec §2). `nodes` are the colliding node keys (encoded),
    /// in key order.
    UniqueViolation {
        /// The primary label the constraint is declared on.
        label: String,
        /// The UNIQUE property whose value collides.
        property: String,
        /// The shared value (canonical encoding).
        value: Vec<u8>,
        /// The colliding nodes' keys (encoded), in key order.
        nodes: Vec<Vec<u8>>,
    },
}

/// One conflict surfaced by a merge: a cell-level clash (same key edited
/// incompatibly on both sides) or a graph-level violation introduced by an
/// otherwise map-clean merge. This is spec §7's structured conflict record —
/// "key and base/ours/theirs values, or a violation class for graph-level
/// conflicts". A single merge yields conflicts of one kind: cell conflicts
/// short-circuit before the merged graph exists to validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeConflict {
    /// A cell-level conflict on one map key.
    Cell(CellConflict),
    /// A graph-level referential or constraint violation.
    Graph(GraphViolation),
}

/// The outcome of a graph-level three-way merge.
#[derive(Debug)]
pub enum ManifestMerge {
    /// A clean, graph-validated merge: the merged manifest, with `edges_rev`
    /// rebuilt from the merged forward map and no conflicts.
    Clean(Box<Manifest>),
    /// Conflicts (cell-level, or graph-level from post-merge validation).
    Conflicts {
        /// The partially-merged manifest: conflicted keys are absent (cell
        /// conflicts merge every non-conflicted key), or the graph-invalid
        /// merge (graph violations). Its `conflicts` field is `None`;
        /// populating the persisted conflicts map is the commit-graph
        /// wrapper's job (acetone-14c.4).
        merged: Box<Manifest>,
        /// The conflicts, in category-then-key order.
        conflicts: Vec<MergeConflict>,
    },
}

/// Three-way merge of graph manifests `ours` and `theirs` against their
/// common `base`. Deterministic and symmetric for a clean merge: the merged
/// roots depend only on the three inputs' contents, not on which side is
/// "ours" (Invariant #4). All three must share the repository's chunk
/// parameters.
///
/// A map-clean result is then graph-validated ([`validate_merged`]); any
/// dangling edge or constraint breach the merge introduces demotes it to
/// [`ManifestMerge::Conflicts`] carrying [`GraphViolation`]s.
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
    let mut cells = Vec::new();

    let schema = merge_one(
        store,
        ConflictMap::Schema,
        |m| &m.schema,
        base,
        ours,
        theirs,
        &mut cells,
    )?;
    let nodes = merge_one(
        store,
        ConflictMap::Nodes,
        |m| &m.nodes,
        base,
        ours,
        theirs,
        &mut cells,
    )?;
    let edges_fwd = merge_one(
        store,
        ConflictMap::Edges,
        |m| &m.edges_fwd,
        base,
        ours,
        theirs,
        &mut cells,
    )?;

    // `edges_rev` is derived: rebuild it from the merged forward map rather
    // than merging it, so forward and reverse can never diverge (Invariant
    // #5) — including on the cell-conflict path, where the conflicted edges
    // are absent from both maps. Secondary `indexes` are likewise derived;
    // there are none before Phase 5, and they are rebuilt when they arrive.
    let edges_rev = rebuild_reverse(store, &edges_fwd, params)?;

    let merged = Manifest {
        chunk_params: params,
        schema: MapRoot::from_root(&schema),
        nodes: MapRoot::from_root(&nodes),
        edges_fwd: MapRoot::from_root(&edges_fwd),
        edges_rev: MapRoot::from_root(&edges_rev),
        indexes: Default::default(),
        conflicts: None,
    };

    // Cell conflicts short-circuit graph validation: the merged graph is
    // partial (conflicted keys absent), so referential/constraint checks
    // would be over an incomplete graph. The partial manifest is returned so
    // the wrapper can persist the conflicts as a merge-in-progress workspace.
    if !cells.is_empty() {
        return Ok(ManifestMerge::Conflicts {
            merged: Box::new(merged),
            conflicts: cells.into_iter().map(MergeConflict::Cell).collect(),
        });
    }

    // Referential integrity and schema constraints can be broken by an
    // otherwise map-clean merge (acetone-14c.3); surface any breach as data.
    let violations = validate_merged(store, base, &merged)?;
    if !violations.is_empty() {
        return Ok(ManifestMerge::Conflicts {
            merged: Box::new(merged),
            conflicts: violations.into_iter().map(MergeConflict::Graph).collect(),
        });
    }

    Ok(ManifestMerge::Clean(Box::new(merged)))
}

/// Three-way merge one map, appending any cell conflicts (tagged with `map`).
fn merge_one<S: ChunkStore>(
    store: &S,
    map: ConflictMap,
    select: fn(&Manifest) -> &MapRoot,
    base: &Manifest,
    ours: &Manifest,
    theirs: &Manifest,
    conflicts: &mut Vec<CellConflict>,
) -> Result<Root, GraphError> {
    let params = base.chunk_params;
    let outcome = prolly_merge(
        store,
        &select(base).to_root(params)?,
        &select(ours).to_root(params)?,
        &select(theirs).to_root(params)?,
    )?;
    for c in outcome.conflicts {
        conflicts.push(CellConflict {
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

/// Validate a map-clean merged manifest against `base` (acetone-14c.3):
/// referential integrity (no dangling edges) and schema constraints
/// (existence, UNIQUE), re-checked over the keys the merge changed. Returns
/// the violations in category order (dangling edges, then existence, then
/// UNIQUE), each category in key order — deterministic, so the merge stays
/// a pure function of its inputs (Invariant #4).
///
/// Only **merge-introduced** breaches are reported: a violation is attributed
/// to the merge when it arises from a key the merge changed (an added edge, a
/// deleted endpoint, an added/modified node) or a constraint the merge newly
/// tightened. A breach already present in `base` that neither side touched is
/// left alone — the merge did not cause it, and re-reporting it would attach
/// unrelated history to this merge.
fn validate_merged<S: ChunkStore>(
    store: &S,
    base: &Manifest,
    merged: &Manifest,
) -> Result<Vec<GraphViolation>, GraphError> {
    let params = merged.chunk_params;
    let base_nodes = base.nodes.to_root(params)?;
    let merged_nodes = merged.nodes.to_root(params)?;
    let base_edges = base.edges_fwd.to_root(params)?;
    let merged_edges = merged.edges_fwd.to_root(params)?;

    // Node changes base -> merged: deletions (endpoints that may now dangle)
    // and additions/modifications (nodes to re-validate against constraints).
    let mut deleted_nodes: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut changed_nodes: BTreeSet<Vec<u8>> = BTreeSet::new();
    for entry in prolly_diff(store, &base_nodes, &merged_nodes)? {
        let entry = entry?;
        match (entry.before.is_some(), entry.after.is_some()) {
            (true, false) => {
                deleted_nodes.insert(entry.key.to_vec());
            }
            (_, true) => {
                changed_nodes.insert(entry.key.to_vec());
            }
            (false, false) => {}
        }
    }
    // Forward edges the merge added (may reference an absent endpoint).
    let mut added_edges: BTreeSet<Vec<u8>> = BTreeSet::new();
    for entry in prolly_diff(store, &base_edges, &merged_edges)? {
        let entry = entry?;
        if entry.before.is_none() && entry.after.is_some() {
            added_edges.insert(entry.key.to_vec());
        }
    }

    let mut violations = Vec::new();

    // --- Referential integrity: dangling forward edges ---
    for item in scan(store, &merged_edges, ..)? {
        let (raw_key, _) = item?;
        let edge = EdgeKey::decode_fwd(&raw_key)?;
        let is_added = added_edges.contains(raw_key.as_ref());
        for role in [Endpoint::Src, Endpoint::Dst] {
            let endpoint = match role {
                Endpoint::Src => edge.src(),
                Endpoint::Dst => edge.dst(),
            };
            let endpoint_enc = endpoint.encode()?;
            if get(store, &merged_nodes, &endpoint_enc)?.is_none()
                && (is_added || deleted_nodes.contains(&endpoint_enc))
            {
                violations.push(GraphViolation::DanglingEdge {
                    edge: raw_key.to_vec(),
                    endpoint: endpoint_enc,
                    role,
                });
            }
        }
    }

    // --- Schema constraints ---
    let labels = label_defs(store, &merged.schema.to_root(params)?)?;
    let base_labels = label_defs(store, &base.schema.to_root(params)?)?;
    // Load the merged nodes once for existence and UNIQUE checks.
    let mut all_nodes: Vec<(NodeKey, NodeRecord)> = Vec::new();
    for item in scan(store, &merged_nodes, ..)? {
        let (key, value) = item?;
        all_nodes.push((NodeKey::decode(&key)?, NodeRecord::decode(&value)?));
    }

    // Existence: a required property is present iff it is a key property
    // (always present, by identity) or in the node record. Report a breach
    // when the node changed, or the constraint is newly required by the
    // merged schema (a tightening that pre-existing nodes may not satisfy).
    for (key, record) in &all_nodes {
        let Some(def) = labels.get(key.label()) else {
            continue;
        };
        let key_enc = key.encode()?;
        let changed = changed_nodes.contains(&key_enc);
        for property in def.exists() {
            let present = def.key().iter().any(|k| k == property)
                || record.properties().contains_key(property);
            if present {
                continue;
            }
            let newly_required = base_labels
                .get(key.label())
                .is_none_or(|b| !b.exists().iter().any(|e| e == property));
            if changed || newly_required {
                violations.push(GraphViolation::MissingRequired {
                    node: key_enc.clone(),
                    property: property.clone(),
                });
            }
        }
    }

    // UNIQUE: group merged nodes by (label, property, value); a group of two
    // or more is a collision. Report it when the merge is responsible — a
    // colliding node changed, or the constraint is newly declared.
    let mut groups: BTreeMap<(String, String, Vec<u8>), Vec<Vec<u8>>> = BTreeMap::new();
    for (key, record) in &all_nodes {
        let Some(def) = labels.get(key.label()) else {
            continue;
        };
        for property in def.unique() {
            if let Some(value) = record.properties().get(property) {
                let value_enc = encode_value(value).map_err(RecordEncodeError::from)?;
                groups
                    .entry((key.label().to_owned(), property.clone(), value_enc))
                    .or_default()
                    .push(key.encode()?);
            }
        }
    }
    for ((label, property, value), nodes) in groups {
        if nodes.len() < 2 {
            continue;
        }
        let newly_unique = base_labels
            .get(&label)
            .is_none_or(|b| !b.unique().iter().any(|u| u == &property));
        let touched = nodes.iter().any(|n| changed_nodes.contains(n));
        if newly_unique || touched {
            violations.push(GraphViolation::UniqueViolation {
                label,
                property,
                value,
                nodes,
            });
        }
    }

    Ok(violations)
}

/// Read the label definitions of a `schema` map root, keyed by label name.
fn label_defs<S: ChunkStore>(
    store: &S,
    schema: &Root,
) -> Result<BTreeMap<String, LabelDef>, GraphError> {
    let mut labels = BTreeMap::new();
    for item in scan(store, schema, ..)? {
        let (key, value) = item?;
        if let SchemaEntry::Label { name, def } = SchemaEntry::decode(&key, &value)? {
            labels.insert(name, def);
        }
    }
    Ok(labels)
}
