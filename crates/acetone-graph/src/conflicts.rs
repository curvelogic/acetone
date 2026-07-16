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
        /// The conflicted property (ADR-0035, cell-wise merge), or `None` for a
        /// whole-record conflict (a schema key, or a node/edge whose existence
        /// is disputed by delete-vs-modify).
        property: Option<String>,
    },
    /// A graph-level violation (dangling edge / constraint). Not persisted or
    /// resolvable yet — a violating merge leaves the repository unchanged
    /// (acetone-mws); this variant exists for completeness.
    Graph,
}

/// Append a length-prefixed field, so a run of them cannot alias.
fn push_field(key: &mut Vec<u8>, bytes: &[u8]) {
    key.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    key.extend_from_slice(bytes);
}

/// The `[KIND_CELL][map_tag][key_len][key]` prefix every conflict entry for a
/// `(map, key)` pair shares — a node/edge may carry several per-property
/// conflicts, all resolved together when the key is written. The map key is
/// length-prefixed so the property suffix cannot alias into it.
fn cell_key_prefix(map: ConflictMap, key: &[u8]) -> Vec<u8> {
    let mut prefix = Vec::with_capacity(2 + 4 + key.len());
    prefix.push(KIND_CELL);
    prefix.push(map_tag(map));
    push_field(&mut prefix, key);
    prefix
}

/// The persisted-map entry key for one conflict.
fn entry_key(conflict: &MergeConflict) -> Vec<u8> {
    match conflict {
        MergeConflict::Cell(cell) => {
            // `[…prefix…][prop_present:u8][if present: len-prefixed property]`,
            // so all of a key's per-property conflicts sort together after the
            // shared prefix and none aliases another.
            let mut key = cell_key_prefix(cell.map, &cell.key);
            match &cell.property {
                Some(property) => {
                    key.push(1);
                    push_field(&mut key, property.as_bytes());
                }
                None => key.push(0),
            }
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

/// Read a big-endian `u32` length prefix and the field it introduces from
/// `bytes` at `pos`, advancing `pos` past both.
fn read_field<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len_end = pos.checked_add(4)?;
    let len = u32::from_be_bytes(bytes.get(*pos..len_end)?.try_into().ok()?) as usize;
    let field_end = len_end.checked_add(len)?;
    let field = bytes.get(len_end..field_end)?;
    *pos = field_end;
    Some(field)
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
            let mut pos = 2;
            let map_key = read_field(key, &mut pos).ok_or_else(invalid)?.to_vec();
            let property = match key.get(pos).copied().ok_or_else(invalid)? {
                0 => None,
                1 => {
                    pos += 1;
                    let bytes = read_field(key, &mut pos).ok_or_else(invalid)?;
                    Some(String::from_utf8(bytes.to_vec()).map_err(|_| invalid())?)
                }
                _ => return Err(invalid()),
            };
            Ok(PersistedConflict::Cell {
                map,
                key: map_key,
                property,
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

/// Clear the cell-conflict entries for the `written` `(map, key)` pairs from
/// the conflicts map — a write to a conflicted key resolves it (spec §6,
/// acetone-14c.4c). A single node/edge may carry several per-property conflicts
/// (ADR-0035); writing the record resolves the record, so *every* entry sharing
/// that key's prefix is dropped. Clearing a key that is not a conflict is a
/// harmless no-op, so callers can pass every key they wrote. Returns the reduced
/// root, or `None` when no conflicts remain (the merge is fully resolved).
pub fn clear_written<S: ChunkStore>(
    store: &S,
    root: &Root,
    written: &[(ConflictMap, Vec<u8>)],
) -> Result<Option<Root>, GraphError> {
    let prefixes: Vec<Vec<u8>> = written
        .iter()
        .map(|(map, key)| cell_key_prefix(*map, key))
        .collect();
    // Delete every persisted entry whose bytes begin with a written key's
    // prefix. A scan is needed because per-property entries share a prefix but
    // are distinct exact keys; `apply_batch` deletes only exact keys.
    let mut ops: Vec<BatchOp> = Vec::new();
    for item in scan(store, root, ..)? {
        let (entry, _) = item?;
        if prefixes.iter().any(|p| entry.starts_with(p)) {
            ops.push(BatchOp::Delete(entry.to_vec()));
        }
    }
    let reduced = apply_batch(store, root, ops)?;
    // Any entries left?
    match scan(store, &reduced, ..)?.next() {
        None => Ok(None),
        Some(_) => Ok(Some(reduced)),
    }
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
            property: None,
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
            property: None,
            base: None,
            ours: None,
            theirs: None,
        });
        assert_ne!(entry_key(&cell), entry_key(&schema_cell));
    }

    /// Per-property conflicts on one node round-trip through the entry key, and
    /// stay distinct from each other and from a whole-record conflict on the
    /// same key. A key-prefix collision here would let one property's
    /// resolution silently clear another's.
    #[test]
    fn per_property_cell_entries_are_distinct_and_decode() {
        let key = vec![7, 7];
        let owner = MergeConflict::Cell(CellConflict {
            map: ConflictMap::Nodes,
            key: key.clone(),
            property: Some("owner".into()),
            base: None,
            ours: None,
            theirs: None,
        });
        let os = MergeConflict::Cell(CellConflict {
            map: ConflictMap::Nodes,
            key: key.clone(),
            property: Some("os".into()),
            base: None,
            ours: None,
            theirs: None,
        });
        let whole = MergeConflict::Cell(CellConflict {
            map: ConflictMap::Nodes,
            key: key.clone(),
            property: None,
            base: None,
            ours: None,
            theirs: None,
        });
        assert_ne!(entry_key(&owner), entry_key(&os));
        assert_ne!(entry_key(&owner), entry_key(&whole));

        assert_eq!(
            decode_entry(&entry_key(&owner)).unwrap(),
            PersistedConflict::Cell {
                map: ConflictMap::Nodes,
                key: key.clone(),
                property: Some("owner".into()),
            }
        );
        assert_eq!(
            decode_entry(&entry_key(&whole)).unwrap(),
            PersistedConflict::Cell {
                map: ConflictMap::Nodes,
                key,
                property: None,
            }
        );
    }

    /// A property name whose bytes end in a length-prefix-shaped run must not
    /// alias a different (key, property) split — the length-prefixed map key
    /// pins the boundary.
    #[test]
    fn key_and_property_boundary_is_unambiguous() {
        let a = MergeConflict::Cell(CellConflict {
            map: ConflictMap::Nodes,
            key: vec![1],
            property: Some("ab".into()),
            base: None,
            ours: None,
            theirs: None,
        });
        let b = MergeConflict::Cell(CellConflict {
            map: ConflictMap::Nodes,
            key: vec![1, b'a'],
            property: Some("b".into()),
            base: None,
            ours: None,
            theirs: None,
        });
        assert_ne!(entry_key(&a), entry_key(&b));
        assert_eq!(
            decode_entry(&entry_key(&a)).unwrap(),
            PersistedConflict::Cell {
                map: ConflictMap::Nodes,
                key: vec![1],
                property: Some("ab".into()),
            }
        );
    }
}
