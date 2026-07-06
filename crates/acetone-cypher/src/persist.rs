//! Persist a query's [`WriteChanges`] into the graph store (acetone-mex.2).
//!
//! The read path turns stored nodes into runtime [`NodeValue`]s with an
//! opaque [`EntityId`] (the memcomparable encoding of the node key); the
//! write path produces a net set of final entity states. This module maps
//! those back to storage identities and replays them into a graph
//! [`Transaction`]:
//!
//! - a created/modified node's identity is *derived* from its primary
//!   label's declared key (Load-Bearing Invariant #3): the first label that
//!   declares a key is primary, its key properties form the node key, the
//!   rest are the record. A node with no keyed label cannot be persisted.
//! - a base node referenced by a relationship carries a storage-derived
//!   `EntityId`, which decodes straight back to its `NodeKey`; a created
//!   node carries an overlay id, resolved through a map built from the
//!   upserted nodes.
//!
//! The transaction stages every put/delete and advances the workspace
//! atomically on `save`/`commit`, maintaining `edges_rev` by construction
//! (spec §3.3). CREATE-of-an-existing-key rejection and other constraints
//! are enforced by the constraint bead (acetone-mex.3); this layer upserts.

use std::collections::{BTreeMap, HashMap};

use acetone_graph::GraphError;
use acetone_graph::repo::Transaction;
use acetone_model::Value as ModelValue;
use acetone_model::graph_keys::{EdgeKey, GraphKeyError, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};

use crate::bind::Catalogue;
use crate::exec::WriteChanges;
use crate::exec::value::{EntityId, NodeValue, RelValue, Value};

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("cannot persist node: {0}")]
    Identity(String),
    #[error("cannot persist a {0} as a stored property value")]
    Value(&'static str),
    #[error(transparent)]
    Key(#[from] GraphKeyError),
    #[error(transparent)]
    Graph(#[from] GraphError),
}

/// Replay `changes` into `txn`, deriving storage identities against
/// `catalogue` (the workspace schema). The caller commits or saves the
/// transaction.
pub fn persist_changes(
    changes: &WriteChanges,
    txn: &mut Transaction<'_>,
    catalogue: &Catalogue,
) -> Result<(), PersistError> {
    // Derive every upserted node's key first, so relationship endpoints
    // that reference a freshly-created node (an overlay id) resolve.
    let mut entity_to_key: HashMap<EntityId, NodeKey> = HashMap::new();
    let mut node_records: Vec<(NodeKey, NodeRecord)> =
        Vec::with_capacity(changes.upserted_nodes.len());
    for node in &changes.upserted_nodes {
        let (key, record) = node_key_and_record(node, catalogue)?;
        entity_to_key.insert(node.id.clone(), key.clone());
        node_records.push((key, record));
    }

    for (key, record) in &node_records {
        txn.put_node(key, record)?;
    }
    for rel in &changes.upserted_rels {
        let edge = edge_key(rel, &entity_to_key)?;
        let record = EdgeRecord::new(convert_map(&rel.properties)?);
        txn.put_edge(&edge, &record)?;
    }
    for rel in &changes.deleted_rels {
        let edge = edge_key(rel, &entity_to_key)?;
        txn.delete_edge(&edge)?;
    }
    for id in &changes.deleted_nodes {
        // A deleted base node's id is the memcomparable node-key encoding.
        let key = NodeKey::decode(id.0.as_ref())?;
        txn.delete_node(&key)?;
    }
    Ok(())
}

/// Derive `(NodeKey, NodeRecord)` from a runtime node, using the schema to
/// find the primary label and its key properties.
fn node_key_and_record(
    node: &NodeValue,
    catalogue: &Catalogue,
) -> Result<(NodeKey, NodeRecord), PersistError> {
    // Primary label: the first that declares a (non-empty) key.
    let primary = node
        .labels
        .iter()
        .find(|label| {
            catalogue
                .label(label)
                .is_some_and(|def| !def.key().is_empty())
        })
        .ok_or_else(|| {
            PersistError::Identity(format!(
                "a node with labels {:?} has no label declaring a key; \
                 identity is undefined (Invariant #3)",
                node.labels
            ))
        })?;
    let key_names = catalogue
        .label(primary)
        .expect("primary label was just found in the catalogue")
        .key()
        .to_vec();

    let mut key_values = Vec::with_capacity(key_names.len());
    for name in &key_names {
        let value = node.properties.get(name).ok_or_else(|| {
            PersistError::Identity(format!("node {primary:?} is missing key property {name:?}"))
        })?;
        key_values.push(convert_value(value)?);
    }
    let node_key = NodeKey::new(primary.clone(), key_values)?;

    // Secondary labels: every label but the primary.
    let secondary: Vec<String> = node
        .labels
        .iter()
        .filter(|label| *label != primary)
        .cloned()
        .collect();
    // The record stores only the non-key properties (the key is the key).
    let mut properties = BTreeMap::new();
    for (name, value) in &node.properties {
        if key_names.iter().any(|k| k == name) {
            continue;
        }
        properties.insert(name.clone(), convert_value(value)?);
    }
    Ok((node_key, NodeRecord::new(secondary, properties)))
}

/// Build an edge key from a relationship, resolving its endpoints to node
/// keys — a created node through `entity_to_key`, a base node by decoding
/// its storage-derived id. The discriminator defaults to null (parallel
/// edges need a schema discriminator, out of scope here).
fn edge_key(
    rel: &RelValue,
    entity_to_key: &HashMap<EntityId, NodeKey>,
) -> Result<EdgeKey, PersistError> {
    let resolve = |id: &EntityId| -> Result<NodeKey, PersistError> {
        if let Some(key) = entity_to_key.get(id) {
            return Ok(key.clone());
        }
        Ok(NodeKey::decode(id.0.as_ref())?)
    };
    let src = resolve(&rel.start)?;
    let dst = resolve(&rel.end)?;
    Ok(EdgeKey::new(
        src,
        rel.rel_type.clone(),
        dst,
        ModelValue::Null,
    )?)
}

fn convert_map(
    properties: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, ModelValue>, PersistError> {
    let mut out = BTreeMap::new();
    for (name, value) in properties {
        out.insert(name.clone(), convert_value(value)?);
    }
    Ok(out)
}

/// Convert a runtime value to a storable model value. Maps, nodes,
/// relationships and paths are not storable property values.
fn convert_value(value: &Value) -> Result<ModelValue, PersistError> {
    Ok(match value {
        Value::Null => ModelValue::Null,
        Value::Bool(b) => ModelValue::Bool(*b),
        Value::Int(n) => ModelValue::Int(*n),
        Value::Float(x) => ModelValue::Float(*x),
        Value::String(s) => ModelValue::String(s.clone()),
        Value::List(items) => ModelValue::List(
            items
                .iter()
                .map(convert_value)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Map(_) => return Err(PersistError::Value("map")),
        Value::Node(_) => return Err(PersistError::Value("node")),
        Value::Relationship(_) => return Err(PersistError::Value("relationship")),
        Value::Path(_) => return Err(PersistError::Value("path")),
    })
}
