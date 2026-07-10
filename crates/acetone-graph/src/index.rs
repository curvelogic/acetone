//! Declared property index maintenance (spec §3.3, Invariant #5).
//!
//! The `idx/<name>` maps are **derived** from `nodes`: they MUST be consistent
//! with `nodes` in every committed manifest and MUST be reproducible by
//! `reindex` to identical roots. Both the transactional maintenance in the
//! write path and the from-scratch rebuild go through the single
//! [`index_entry_key`] function, so they cannot diverge — and because the
//! prolly root is a pure function of the final key set (Invariant #1),
//! incremental maintenance and a full rebuild yield identical roots whenever
//! the index is consistent. `fsck` uses the same function to check that.
//!
//! Indexes are **null-blind and NaN-blind**: a node whose indexed property is
//! absent, null, or an unencodable value (NaN, ADR-0004) contributes no entry.
//! This loses nothing for the predicates an index serves — openCypher equality
//! and range comparisons are never true for null or NaN.
//!
//! Incremental maintenance only revisits the nodes a transaction touched, so it
//! assumes a label's declared key tuple is stable across the index's lifetime
//! (redeclaring a key changes identity, an unsupported mutation). Should it ever
//! drift, `fsck` recomputes with the current schema and flags the divergence,
//! and `reindex` repairs it.

use std::collections::BTreeMap;

use acetone_model::Value;
use acetone_model::graph_keys::{GraphKeyError, IndexEntry, NodeKey};
use acetone_model::keys::KeyEncodeError;
use acetone_model::manifest::{Manifest, MapRoot};
use acetone_model::records::NodeRecord;
use acetone_model::schema::{IndexDef, SchemaEntry};
use acetone_prolly::{BatchOp, ChunkParams};
use acetone_store::ChunkStore;

use crate::error::GraphError;

/// A label → its declared key-property tuple. Used to source an indexed value
/// that happens to be a key property (which lives in the node key, not the
/// record, since records exclude key properties — Invariant #3).
pub(crate) type LabelKeys = BTreeMap<String, Vec<String>>;

/// The declared indexes and label key tuples of a schema-entry set.
pub(crate) fn schema_index_info(entries: &[SchemaEntry]) -> (Vec<(String, IndexDef)>, LabelKeys) {
    let mut indexes = Vec::new();
    let mut label_keys = LabelKeys::new();
    for entry in entries {
        match entry {
            SchemaEntry::Index { name, def } => indexes.push((name.clone(), def.clone())),
            SchemaEntry::Label { name, def } => {
                label_keys.insert(name.clone(), def.key().to_vec());
            }
            SchemaEntry::RelType { .. } => {}
        }
    }
    (indexes, label_keys)
}

/// The `idx/<name>` map key a node contributes for one index in a given state,
/// or `None` when it contributes nothing (absent node, does not bear the
/// indexed label, property absent, or a null/unencodable value).
pub(crate) fn index_entry_key(
    node_key: &NodeKey,
    record: Option<&NodeRecord>,
    def: &IndexDef,
    label_keys: &LabelKeys,
) -> Option<Vec<u8>> {
    let record = record?;
    // Does the node bear the indexed label (as primary or secondary)?
    let bears = node_key.label() == def.label()
        || record.secondary_labels().iter().any(|l| l == def.label());
    if !bears {
        return None;
    }
    // Gather every indexed property's value, in declaration order: a key
    // property lives in the node key (by position in the node's *primary* label
    // key tuple); everything else in the record. **Composite null-blind**: if
    // any component is absent or null, the node contributes no entry (an
    // equality that pins all components is never true when one is null).
    let mut values = Vec::with_capacity(def.properties().len());
    for property in def.properties() {
        let value = match label_keys
            .get(node_key.label())
            .and_then(|keys| keys.iter().position(|k| k == property))
        {
            Some(pos) => node_key.key().get(pos).cloned(),
            None => record.properties().get(property).cloned(),
        }?;
        if matches!(value, Value::Null) {
            return None;
        }
        values.push(value);
    }
    let entry = IndexEntry::new(
        def.label(),
        def.properties().to_vec(),
        values,
        node_key.clone(),
    )
    .expect("index label and properties are non-empty (validated at declaration)");
    match entry.encode() {
        Ok(bytes) => Some(bytes),
        // NaN-blind: NaN anywhere in the value — top-level or nested in a list —
        // is unencodable (ADR-0004) and intentionally skipped (NaN comparisons
        // are never true in openCypher).
        Err(GraphKeyError::Encode(KeyEncodeError::NanNotPermitted)) => None,
        Err(_e) => {
            // Any *other* encode failure (temporal range, list depth) is
            // unexpected — surface it in debug/test builds rather than silently
            // dropping the node from the index (which would also fool fsck,
            // since it shares this fn).
            debug_assert!(false, "unexpected index-key encode failure: {_e}");
            None
        }
    }
}

/// A node record read from a map root by its encoded key.
fn record_at<S: ChunkStore>(
    store: &S,
    params: ChunkParams,
    nodes: &MapRoot,
    encoded_key: &[u8],
) -> Result<Option<NodeRecord>, GraphError> {
    let root = nodes.to_root(params)?;
    match acetone_prolly::get(store, &root, encoded_key)? {
        None => Ok(None),
        Some(bytes) => Ok(Some(NodeRecord::decode(&bytes)?)),
    }
}

/// Build one index map from scratch over every node in `nodes` (used for a
/// newly-declared index and by `reindex`).
pub(crate) fn build_full<S: ChunkStore>(
    store: &S,
    params: ChunkParams,
    nodes: &MapRoot,
    def: &IndexDef,
    label_keys: &LabelKeys,
) -> Result<MapRoot, GraphError> {
    let root = nodes.to_root(params)?;
    let mut ops = Vec::new();
    for item in acetone_prolly::scan(store, &root, ..)? {
        let (key, value) = item?;
        let node_key = NodeKey::decode(&key)?;
        let record = NodeRecord::decode(&value)?;
        if let Some(entry) = index_entry_key(&node_key, Some(&record), def, label_keys) {
            ops.push(BatchOp::Put(entry, Vec::new()));
        }
    }
    let empty = acetone_prolly::empty(store, params)?;
    Ok(MapRoot::from_root(&acetone_prolly::apply_batch(
        store, &empty, ops,
    )?))
}

/// Rebuild every declared index map for `manifest` from its `nodes` map.
/// Deterministic and history-independent, so it reproduces exactly the roots
/// incremental maintenance would (Invariant #5).
pub(crate) fn rebuild_all<S: ChunkStore>(
    store: &S,
    manifest: &Manifest,
    entries: &[SchemaEntry],
) -> Result<BTreeMap<String, MapRoot>, GraphError> {
    let (index_defs, label_keys) = schema_index_info(entries);
    let params = manifest.chunk_params;
    let mut out = BTreeMap::new();
    for (name, def) in index_defs {
        out.insert(
            name,
            build_full(store, params, &manifest.nodes, &def, &label_keys)?,
        );
    }
    Ok(out)
}

/// Compute the new index map roots after a transaction's node changes.
///
/// `touched` are the encoded node keys written this transaction; `base_nodes`
/// is the pre-transaction `nodes` root and `new_nodes` the post-transaction
/// one. An index already present in `base_indexes` is updated by delta over the
/// touched keys; an index newly declared this transaction (no base root) is
/// built in full from `new_nodes`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn maintain<S: ChunkStore>(
    store: &S,
    params: ChunkParams,
    base_nodes: &MapRoot,
    new_nodes: &MapRoot,
    touched: &[Vec<u8>],
    base_indexes: &BTreeMap<String, MapRoot>,
    index_defs: &[(String, IndexDef)],
    label_keys: &LabelKeys,
) -> Result<BTreeMap<String, MapRoot>, GraphError> {
    // Read each touched node's before/after record once, decoding its key.
    let mut keys: Vec<Vec<u8>> = touched.to_vec();
    keys.sort();
    keys.dedup();
    let mut touched_states = Vec::with_capacity(keys.len());
    for encoded in &keys {
        let node_key = NodeKey::decode(encoded)?;
        let before = record_at(store, params, base_nodes, encoded)?;
        let after = record_at(store, params, new_nodes, encoded)?;
        touched_states.push((node_key, before, after));
    }

    let mut out = BTreeMap::new();
    for (name, def) in index_defs {
        match base_indexes.get(name) {
            Some(base_root) => {
                let mut ops = Vec::new();
                for (node_key, before, after) in &touched_states {
                    let old_entry = index_entry_key(node_key, before.as_ref(), def, label_keys);
                    let new_entry = index_entry_key(node_key, after.as_ref(), def, label_keys);
                    if old_entry != new_entry {
                        if let Some(k) = old_entry {
                            ops.push(BatchOp::Delete(k));
                        }
                        if let Some(k) = new_entry {
                            ops.push(BatchOp::Put(k, Vec::new()));
                        }
                    }
                }
                let root = if ops.is_empty() {
                    *base_root
                } else {
                    MapRoot::from_root(&acetone_prolly::apply_batch(
                        store,
                        &base_root.to_root(params)?,
                        ops,
                    )?)
                };
                out.insert(name.clone(), root);
            }
            None => {
                out.insert(
                    name.clone(),
                    build_full(store, params, new_nodes, def, label_keys)?,
                );
            }
        }
    }
    Ok(out)
}
