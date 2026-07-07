//! The workspace `conflicts` map of a merge-in-progress (spec §6,
//! acetone-14c.4). A conflicted merge does not commit; instead it records
//! *which* keys conflict in the manifest's `conflicts` map and enters a
//! merge-in-progress state (a `MERGE_HEAD` ref names `theirs`). The conflict
//! values themselves are **not** stored — they are re-derived on demand from
//! the `ours` (branch tip) and `theirs` (`MERGE_HEAD`) manifests, so the map
//! stays a compact, deterministic index.
//!
//! Each entry's key is `[kind][detail…]` so entries are unique and scan in a
//! stable order; the value is empty. `kind` distinguishes a cell conflict
//! (the same map key edited incompatibly on both sides — resolvable by
//! picking a side) from a graph-level violation (dangling edge / constraint,
//! resolved by ordinary writes; acetone-14c.4c).

use acetone_prolly::{BatchOp, ChunkParams, Root, apply_batch, empty, scan};
use acetone_store::ChunkStore;

use crate::error::GraphError;
use crate::merge::{ConflictMap, Endpoint, GraphViolation, MergeConflict};

const KIND_CELL: u8 = 0;
const KIND_GRAPH: u8 = 1;

const MAP_SCHEMA: u8 = 0;
const MAP_NODES: u8 = 1;
const MAP_EDGES: u8 = 2;

fn map_tag(map: ConflictMap) -> u8 {
    match map {
        ConflictMap::Schema => MAP_SCHEMA,
        ConflictMap::Nodes => MAP_NODES,
        ConflictMap::Edges => MAP_EDGES,
    }
}

fn map_from_tag(tag: u8) -> Option<ConflictMap> {
    match tag {
        MAP_SCHEMA => Some(ConflictMap::Schema),
        MAP_NODES => Some(ConflictMap::Nodes),
        MAP_EDGES => Some(ConflictMap::Edges),
        _ => None,
    }
}

/// A conflict read back from the persisted map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedConflict {
    /// A cell conflict: the key in `map` was edited incompatibly on both
    /// sides. Resolvable by picking a side (its value is re-derived from the
    /// ours/theirs manifests).
    Cell {
        /// Which working map the key belongs to.
        map: ConflictMap,
        /// The conflicted key (encoded).
        key: Vec<u8>,
    },
    /// A graph-level violation (dangling edge / constraint). Resolved by
    /// ordinary writes, not by picking a side (acetone-14c.4c).
    Graph,
}

/// Append a length-prefixed field, so a run of them cannot alias.
fn push_field(key: &mut Vec<u8>, bytes: &[u8]) {
    key.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    key.extend_from_slice(bytes);
}

/// The persisted-map entry key for one conflict.
fn entry_key(conflict: &MergeConflict) -> Vec<u8> {
    match conflict {
        MergeConflict::Cell(cell) => {
            let mut key = Vec::with_capacity(2 + cell.key.len());
            key.push(KIND_CELL);
            key.push(map_tag(cell.map));
            key.extend_from_slice(&cell.key);
            key
        }
        MergeConflict::Graph(violation) => {
            // Every distinguishing field is length-prefixed so distinct
            // violations never alias to the same entry key (e.g. an edge with
            // *both* endpoints missing yields two DanglingEdge violations that
            // must stay separate). Decode only recovers the kind, so the
            // fields need not be parseable back — only unique.
            let mut key = vec![KIND_GRAPH];
            match violation {
                GraphViolation::DanglingEdge { edge, role, .. } => {
                    key.push(0);
                    key.push(match role {
                        Endpoint::Src => 0,
                        Endpoint::Dst => 1,
                    });
                    push_field(&mut key, edge);
                }
                GraphViolation::MissingRequired { node, property } => {
                    key.push(1);
                    push_field(&mut key, property.as_bytes());
                    push_field(&mut key, node);
                }
                GraphViolation::UniqueViolation {
                    label,
                    property,
                    value,
                    ..
                } => {
                    key.push(2);
                    push_field(&mut key, label.as_bytes());
                    push_field(&mut key, property.as_bytes());
                    push_field(&mut key, value);
                }
            }
            key
        }
    }
}

/// Decode a persisted-map entry key back to a [`PersistedConflict`].
fn decode_entry(key: &[u8]) -> Result<PersistedConflict, GraphError> {
    let invalid = || GraphError::CorruptConflicts {
        reason: "conflicts-map entry key is malformed",
    };
    match key.first().copied() {
        Some(KIND_CELL) => {
            let tag = *key.get(1).ok_or_else(invalid)?;
            let map = map_from_tag(tag).ok_or_else(invalid)?;
            Ok(PersistedConflict::Cell {
                map,
                key: key[2..].to_vec(),
            })
        }
        Some(KIND_GRAPH) => Ok(PersistedConflict::Graph),
        _ => Err(invalid()),
    }
}

/// Build the `conflicts` prolly map for a merge's conflicts. The values are
/// empty; the entry keys carry the whole record (see the module docs).
pub fn build_conflicts_map<S: ChunkStore>(
    store: &S,
    params: ChunkParams,
    conflicts: &[MergeConflict],
) -> Result<Root, GraphError> {
    let ops: Vec<BatchOp> = conflicts
        .iter()
        .map(|c| BatchOp::Put(entry_key(c), Vec::new()))
        .collect();
    let base = empty(store, params)?;
    Ok(apply_batch(store, &base, ops)?)
}

/// Read the conflicts back from a `conflicts` map root, in the map's key
/// order (deterministic).
pub fn read_conflicts<S: ChunkStore>(
    store: &S,
    root: &Root,
) -> Result<Vec<PersistedConflict>, GraphError> {
    let mut out = Vec::new();
    for item in scan(store, root, ..)? {
        let (key, _) = item?;
        out.push(decode_entry(&key)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::CellConflict;

    /// Distinct violations that share a primary entity must not collide on
    /// their entry key (M1 regression): an edge with both endpoints missing
    /// yields a Src and a Dst DanglingEdge on the same edge.
    #[test]
    fn distinct_graph_violations_get_distinct_entry_keys() {
        let src = MergeConflict::Graph(GraphViolation::DanglingEdge {
            edge: vec![1, 2, 3],
            endpoint: vec![9],
            role: Endpoint::Src,
        });
        let dst = MergeConflict::Graph(GraphViolation::DanglingEdge {
            edge: vec![1, 2, 3],
            endpoint: vec![8],
            role: Endpoint::Dst,
        });
        assert_ne!(entry_key(&src), entry_key(&dst));

        // Two missing-required properties on one node stay distinct.
        let a = MergeConflict::Graph(GraphViolation::MissingRequired {
            node: vec![7],
            property: "email".into(),
        });
        let b = MergeConflict::Graph(GraphViolation::MissingRequired {
            node: vec![7],
            property: "name".into(),
        });
        assert_ne!(entry_key(&a), entry_key(&b));
    }

    /// A cell conflict and a graph violation on the same key stay distinct,
    /// and cell keys across maps do not alias.
    #[test]
    fn cell_and_graph_keys_do_not_alias() {
        let cell = MergeConflict::Cell(CellConflict {
            map: ConflictMap::Nodes,
            key: vec![1, 2],
            base: None,
            ours: None,
            theirs: None,
        });
        let node_missing = MergeConflict::Graph(GraphViolation::MissingRequired {
            node: vec![1, 2],
            property: "x".into(),
        });
        assert_ne!(entry_key(&cell), entry_key(&node_missing));

        let schema_cell = MergeConflict::Cell(CellConflict {
            map: ConflictMap::Schema,
            key: vec![1, 2],
            base: None,
            ours: None,
            theirs: None,
        });
        assert_ne!(entry_key(&cell), entry_key(&schema_cell));
    }
}
