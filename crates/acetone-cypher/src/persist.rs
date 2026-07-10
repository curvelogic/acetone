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
//! (spec §3.3). Constraints (spec §2, acetone-mex.3) are checked here
//! against the workspace before the transaction: mandatory single-keyed
//! identity, key immutability, CREATE-of-an-existing-key, existence and
//! UNIQUE. UNIQUE is a base scan (excluding same-transaction deletions) for
//! now: it catches a new value colliding with committed data, but NOT two
//! *new* nodes that collide within one statement — so an unindexed UNIQUE
//! can still admit a violating graph until index-backed enforcement lands
//! (Phase 5, acetone-ryg). Same-transaction deletions are subtracted so the
//! delete-plus-create rekey path is not falsely rejected.

use std::collections::{BTreeMap, HashMap, HashSet};

use acetone_graph::GraphError;
use acetone_graph::repo::{Snapshot, Transaction};
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
    #[error(
        "a node with labels {labels:?} bears more than one label declaring a key ({first:?}, {second:?}); node identity is ambiguous"
    )]
    AmbiguousIdentity {
        labels: Vec<String>,
        first: String,
        second: String,
    },
    #[error(
        "CREATE of {label:?} {key:?} conflicts with an existing node; identity must be unique (use MERGE to upsert)"
    )]
    DuplicateKey { label: String, key: String },
    #[error(
        "CREATE of {rtype} relationship {src} -> {dst} conflicts with an existing edge; acetone \
         v0.1 has no parallel-edge discriminator (ADR-0030) — use MERGE to upsert, or SET to modify"
    )]
    DuplicateEdge {
        /// The relationship type.
        rtype: String,
        /// The source endpoint, rendered.
        src: String,
        /// The destination endpoint, rendered.
        dst: String,
    },
    #[error(
        "SET must not change the key property of {label:?} (node identity is immutable; a key change is a delete-plus-create — see rekey)"
    )]
    KeyImmutable { label: String },
    #[error("node {label:?} {key:?} is missing required property {property:?}")]
    MissingRequired {
        label: String,
        key: String,
        property: String,
    },
    #[error(
        "UNIQUE constraint on {label:?}.{property:?} violated: value already used by another node"
    )]
    UniqueViolation { label: String, property: String },
    #[error("cannot persist a {0} as a stored property value")]
    Value(&'static str),
    #[error(transparent)]
    Key(#[from] GraphKeyError),
    #[error(transparent)]
    Graph(#[from] GraphError),
}

/// Replay `changes` into `txn`, deriving storage identities against
/// `catalogue` (the workspace schema) and enforcing the constraints of
/// spec §2 against `base` (the workspace before this transaction):
/// mandatory keys, key immutability, CREATE-of-an-existing-key,
/// existence and UNIQUE (acetone-mex.3). The caller commits or saves.
pub fn persist_changes(
    changes: &WriteChanges,
    txn: &mut Transaction<'_>,
    catalogue: &Catalogue,
    base: &Snapshot<'_>,
) -> Result<(), PersistError> {
    // Derive every upserted node's key first (so relationship endpoints
    // that reference a freshly-created node resolve), checking identity
    // constraints as we go.
    let mut entity_to_key: HashMap<EntityId, NodeKey> = HashMap::new();
    let mut node_records: Vec<(NodeKey, NodeRecord)> =
        Vec::with_capacity(changes.upserted_nodes.len());
    // Keys written this query, to catch a duplicate-key CREATE within one
    // statement (`CREATE (:L{k:1}) CREATE (:L{k:1})`).
    let mut written_keys: HashSet<Vec<u8>> = HashSet::new();
    // Keys freed by a DELETE in this same transaction — a base node being
    // deleted must not count as a live collision, so a single-statement
    // delete-plus-create (the sanctioned rekey path) is not falsely
    // rejected. A base node's id is exactly its encoded key.
    let deleted_keys: HashSet<Vec<u8>> = changes
        .deleted_nodes
        .iter()
        .map(|id| id.0.to_vec())
        .collect();

    for node in &changes.upserted_nodes {
        // A created node carries an overlay id (its key does not decode); a
        // modified base node carries its storage id (which decodes back to its
        // original, immutable key). For a modified base node, fetch its stored
        // record so unchanged deferred-typed properties are preserved verbatim
        // (ADR-0029 / U2), not retyped to string on write-back.
        let decoded_id = NodeKey::decode(node.id.0.as_ref());
        let base_record = match &decoded_id {
            Ok(original) => base.get_node(original)?,
            Err(_) => None,
        };
        let (key, record) = node_key_and_record(node, catalogue, base_record.as_ref())?;

        match &decoded_id {
            Err(_) => {
                // Created: its key must not already exist (CREATE is not an
                // upsert — that is MERGE), unless that node is being deleted
                // in the same transaction. MERGE-created nodes never reach a
                // pre-existing key, so a collision here is a CREATE conflict.
                if base.get_node(&key)?.is_some() && !deleted_keys.contains(&key.encode()?) {
                    return Err(PersistError::DuplicateKey {
                        label: key.label().to_string(),
                        key: format!("{:?}", key.key()),
                    });
                }
            }
            Ok(original) => {
                // Modified base node: SET must not have changed the key
                // (Invariant #3). Catches the cases the bind-time gate
                // cannot (unlabelled node, parameter map, unknown label).
                if original.encode()? != key.encode()? {
                    return Err(PersistError::KeyImmutable {
                        label: key.label().to_string(),
                    });
                }
            }
        }
        if !written_keys.insert(key.encode()?) {
            return Err(PersistError::DuplicateKey {
                label: key.label().to_string(),
                key: format!("{:?}", key.key()),
            });
        }
        check_constraints(node, &key, catalogue, base, &deleted_keys)?;

        entity_to_key.insert(node.id.clone(), key.clone());
        node_records.push((key, record));
    }

    // Stage deletions BEFORE upserts, so a delete-plus-create of the same
    // key (the rekey path) nets to the create rather than the delete
    // clobbering it: staged ops apply in order per map.
    for rel in &changes.deleted_rels {
        let edge = edge_key(rel, &entity_to_key)?;
        txn.delete_edge(&edge)?;
    }
    for id in &changes.deleted_nodes {
        // A deleted base node's id is the memcomparable node-key encoding.
        let key = NodeKey::decode(id.0.as_ref())?;
        txn.delete_node(&key)?;
    }
    for (key, record) in &node_records {
        txn.put_node(key, record)?;
    }
    // A CREATE cannot make a duplicate/parallel edge: acetone v0.1 has no
    // query-reachable discriminator, so two edges with the same (src, type, dst)
    // share one key. Reject a created edge whose key already exists in the base
    // graph (and is not being deleted this statement) or duplicates another
    // created edge (ADR-0030). MERGE that matched an existing edge never reaches
    // here as a create, and SET on a matched edge is a modification, put below.
    if !changes.created_rels.is_empty() {
        let mut deleted_edge_keys: HashSet<Vec<u8>> = HashSet::new();
        for rel in &changes.deleted_rels {
            deleted_edge_keys.insert(edge_key(rel, &entity_to_key)?.encode_fwd()?);
        }
        // `existing` starts as the base edge keys (minus those freed by a delete
        // this statement) and grows with each created edge, so a collision with
        // either base or an earlier create fails the insert.
        let mut existing: HashSet<Vec<u8>> = HashSet::new();
        for (key, _) in base.edges()? {
            let enc = key.encode_fwd()?;
            if !deleted_edge_keys.contains(&enc) {
                existing.insert(enc);
            }
        }
        for rel in &changes.created_rels {
            let edge = edge_key(rel, &entity_to_key)?;
            if !existing.insert(edge.encode_fwd()?) {
                return Err(PersistError::DuplicateEdge {
                    rtype: edge.rtype().to_string(),
                    src: format!("{}{:?}", edge.src().label(), edge.src().key()),
                    dst: format!("{}{:?}", edge.dst().label(), edge.dst().key()),
                });
            }
            let record = EdgeRecord::new(convert_map(&rel.properties)?);
            txn.put_edge(&edge, &record)?;
        }
    }
    for rel in &changes.modified_rels {
        let edge = edge_key(rel, &entity_to_key)?;
        let record = EdgeRecord::new(convert_map(&rel.properties)?);
        txn.put_edge(&edge, &record)?;
    }
    Ok(())
}

/// Derive `(NodeKey, NodeRecord)` from a runtime node, using the schema to
/// find the primary label and its key properties. Exactly one label must
/// declare a key: none is unidentifiable, two is ambiguous (Invariant #3).
fn node_key_and_record(
    node: &NodeValue,
    catalogue: &Catalogue,
    base_record: Option<&NodeRecord>,
) -> Result<(NodeKey, NodeRecord), PersistError> {
    // Primary label: the one that declares a (non-empty) key.
    let mut keyed = node.labels.iter().filter(|label| {
        catalogue
            .label(label)
            .is_some_and(|def| !def.key().is_empty())
    });
    let primary = keyed.next().ok_or_else(|| {
        PersistError::Identity(format!(
            "a node with labels {:?} has no label declaring a key; \
             identity is undefined (Invariant #3)",
            node.labels
        ))
    })?;
    if let Some(second) = keyed.next() {
        return Err(PersistError::AmbiguousIdentity {
            labels: node.labels.clone(),
            first: primary.clone(),
            second: second.clone(),
        });
    }
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
        // Preserve an unchanged property's stored value verbatim (ADR-0029 / U2):
        // the read path renders deferred types (Bytes/temporal) lossily to
        // strings, so re-persisting the runtime value would retype them. If the
        // property came from a modified base node and its runtime value is
        // exactly the adapter's rendering of the stored value, it was read and
        // written back unchanged — keep the stored `ModelValue`. Otherwise it was
        // added or changed, so convert (lossless for non-deferred types, so this
        // is a no-op for them).
        let model = match base_record.and_then(|r| r.properties().get(name)) {
            Some(stored) if same_rendering(&crate::exec::adapter::convert_value(stored), value) => {
                stored.clone()
            }
            _ => convert_value(value)?,
        };
        properties.insert(name.clone(), model);
    }
    Ok((node_key, NodeRecord::new(secondary, properties)))
}

/// Structural equality on the runtime-value subset a stored node property can
/// take once the read adapter has rendered deferred types (Bytes/temporal) to
/// strings. The runtime [`Value`] has no `PartialEq` (openCypher equality is
/// deliberately three-valued), so this is a plain structural compare used only
/// to decide whether a property was written back unchanged. Any value shape a
/// stored property cannot hold (Node/Rel/Path/Map) compares unequal, which
/// correctly treats it as a change.
fn same_rendering(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same_rendering(a, b))
        }
        _ => false,
    }
}

/// Enforce the schema's existence and UNIQUE constraints (spec §2) for one
/// upserted node against the workspace `base`. UNIQUE is a base scan here —
/// index-backed enforcement (and catching two *new* nodes that collide in
/// one statement) arrives with the secondary indexes of Phase 5.
fn check_constraints(
    node: &NodeValue,
    key: &NodeKey,
    catalogue: &Catalogue,
    base: &Snapshot<'_>,
    deleted_keys: &HashSet<Vec<u8>>,
) -> Result<(), PersistError> {
    let Some(def) = catalogue.label(key.label()) else {
        return Ok(());
    };

    for property in def.exists() {
        if !node.properties.contains_key(property) {
            return Err(PersistError::MissingRequired {
                label: key.label().to_string(),
                key: format!("{:?}", key.key()),
                property: property.clone(),
            });
        }
    }

    let wanted: Vec<(&str, ModelValue)> = def
        .unique()
        .iter()
        .filter_map(|property| {
            node.properties
                .get(property)
                .map(|value| convert_value(value).map(|v| (property.as_str(), v)))
        })
        .collect::<Result<_, _>>()?;
    if wanted.is_empty() {
        return Ok(());
    }
    let this_key = key.encode()?;
    for (other_key, other_record) in base.nodes()? {
        let other_encoded = other_key.encode()?;
        // Skip the node itself and any node being deleted in this
        // transaction (its unique value is freed).
        if other_key.label() != key.label()
            || other_encoded == this_key
            || deleted_keys.contains(&other_encoded)
        {
            continue;
        }
        for (property, value) in &wanted {
            if other_record.properties().get(*property) == Some(value) {
                return Err(PersistError::UniqueViolation {
                    label: key.label().to_string(),
                    property: (*property).to_string(),
                });
            }
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use acetone_model::DateTime;
    use acetone_model::schema::{LabelDef, SchemaEntry};
    use std::collections::BTreeMap;

    /// A catalogue with a single label `L` keyed on `id`.
    fn catalogue() -> Catalogue {
        let def =
            LabelDef::new(vec!["id".to_string()], BTreeMap::new(), [], []).expect("label def");
        crate::exec::catalogue_from_schema(vec![SchemaEntry::Label {
            name: "L".to_string(),
            def,
        }])
    }

    fn runtime(props: &[(&str, Value)]) -> NodeValue {
        NodeValue {
            id: EntityId::from_bytes(b"storage-id".to_vec()),
            labels: vec!["L".to_string()],
            properties: props
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        }
    }

    #[test]
    fn write_back_preserves_unchanged_deferred_properties() {
        // U2 (ADR-0029): the read path renders Bytes/temporal to strings, so a
        // read→modify→write cycle must not retype the untouched ones. `name` is
        // genuinely changed; `data` (Bytes) and `when` (DateTime) are read back
        // exactly as the adapter rendered them.
        let data = ModelValue::Bytes(vec![0xAB, 0xCD, 0xEF]);
        let when = ModelValue::DateTime(DateTime {
            epoch_nanos: 1_600_000_000_000_000_000,
            offset_minutes: 60,
        });
        let base = NodeRecord::new(
            Vec::<String>::new(),
            BTreeMap::from([
                ("name".to_string(), ModelValue::String("old".into())),
                ("data".to_string(), data.clone()),
                ("when".to_string(), when.clone()),
            ]),
        );
        let node = runtime(&[
            ("id", Value::Int(1)),
            ("name", Value::String("new".into())), // changed
            ("data", crate::exec::adapter::convert_value(&data)), // read back unchanged
            ("when", crate::exec::adapter::convert_value(&when)), // read back unchanged
        ]);

        let (key, record) = node_key_and_record(&node, &catalogue(), Some(&base)).expect("persist");
        assert_eq!(
            key,
            NodeKey::new("L", vec![ModelValue::Int(1)]).expect("key")
        );
        assert_eq!(
            record.properties().get("data"),
            Some(&data),
            "unchanged Bytes must keep its stored type, not become a hex string"
        );
        assert_eq!(
            record.properties().get("when"),
            Some(&when),
            "unchanged DateTime must keep its stored type, not become a debug string"
        );
        assert_eq!(
            record.properties().get("name"),
            Some(&ModelValue::String("new".into())),
            "a genuinely changed property is stored as written"
        );
    }

    #[test]
    fn write_back_of_a_changed_deferred_property_stores_the_new_value() {
        // Setting a deferred property to a different value must store the new
        // value, not silently preserve the old one.
        let data = ModelValue::Bytes(vec![0xAB, 0xCD]);
        let base = NodeRecord::new(
            Vec::<String>::new(),
            BTreeMap::from([("data".to_string(), data)]),
        );
        let node = runtime(&[
            ("id", Value::Int(1)),
            ("data", Value::String("changed".into())),
        ]);
        let (_key, record) =
            node_key_and_record(&node, &catalogue(), Some(&base)).expect("persist");
        assert_eq!(
            record.properties().get("data"),
            Some(&ModelValue::String("changed".into()))
        );
    }

    #[test]
    fn a_created_node_has_no_base_and_converts_directly() {
        let node = runtime(&[("id", Value::Int(1)), ("name", Value::String("x".into()))]);
        let (_key, record) = node_key_and_record(&node, &catalogue(), None).expect("persist");
        assert_eq!(
            record.properties().get("name"),
            Some(&ModelValue::String("x".into()))
        );
    }
}
