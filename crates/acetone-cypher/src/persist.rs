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
use acetone_model::display::{format_key_tuple, format_label, format_labels, format_node_identity};
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
        "CREATE of {label:?} {key} conflicts with an existing node; CREATE always makes a new \
         node, so identity collides (a MERGE on a multi-element pattern also CREATEs its nodes \
         when the whole pattern doesn't match). To match-or-create, MERGE each node on its own \
         before MERGEing a relationship between them: \
         `MERGE (a:Label {{…}}) MERGE (b:…) MERGE (a)-[:…]->(b)`"
    )]
    DuplicateKey { label: String, key: String },
    #[error(
        "CREATE creates the node {label:?} {key} twice in the same statement; each CREATE makes a \
         new node and identity must be unique"
    )]
    DuplicateKeyInStatement { label: String, key: String },
    #[error(
        "cannot add the {rtype} relationship {src} -> {dst}: it conflicts with an existing relationship \
         and acetone v0.1 has no parallel-edge discriminator (ADR-0030) — modify the existing relationship \
         with SET, or delete it first"
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
    #[error("node {label:?} {key} is missing required property {property:?}")]
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
        // original, immutable key). Unchanged deferred-typed properties survive
        // write-back because the read adapter carries them as `Value::Stored`
        // (ADR-0038), so no base-record re-read is needed.
        let decoded_id = NodeKey::decode(node.id.0.as_ref());
        let (key, record) = node_key_and_record(node, catalogue)?;

        match &decoded_id {
            Err(_) => {
                // Created: its key must not already exist (CREATE is not an
                // upsert — that is MERGE), unless that node is being deleted
                // in the same transaction. A MERGE on a multi-element pattern
                // that fails to match as a whole also CREATEs its nodes, so it
                // too can reach a pre-existing key here.
                if base.get_node(&key)?.is_some() && !deleted_keys.contains(&key.encode()?) {
                    return Err(PersistError::DuplicateKey {
                        label: key.label().to_string(),
                        key: format_key_tuple(key.key()),
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
            return Err(PersistError::DuplicateKeyInStatement {
                label: key.label().to_string(),
                key: format_key_tuple(key.key()),
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
                    src: format_node_identity(edge.src().label(), edge.src().key()),
                    dst: format_node_identity(edge.dst().label(), edge.dst().key()),
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
) -> Result<(NodeKey, NodeRecord), PersistError> {
    // Primary label: the one that declares a (non-empty) key.
    let mut keyed = node.labels.iter().filter(|label| {
        catalogue
            .label(label)
            .is_some_and(|def| !def.key().is_empty())
    });
    let primary = keyed.next().ok_or_else(|| {
        if node.labels.is_empty() {
            // A bare `(Topic {…})` parses `Topic` as a variable, not a label,
            // so the node reaches here with zero labels — almost always the
            // missing-colon mistake.
            PersistError::Identity(
                "this node has no labels — in Cypher a label is written with a colon, \
                 e.g. `(:Topic {…})`; a bare name like `(Topic {…})` is a variable, \
                 not a label"
                    .to_string(),
            )
        } else {
            // Labels are present but none of them declares a key, so identity
            // is undefined (Invariant #3): declare a key on one of them.
            PersistError::Identity(format!(
                "none of the labels {} declares a key, so this node has no identity \
                 (Invariant #3) — declare one first, e.g. \
                 `acetone declare-label {} --key <property>`",
                format_labels(&node.labels),
                format_label(&node.labels[0]),
            ))
        }
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
    // The record stores only the non-key properties (the key is the key). A
    // deferred-typed property (Bytes/temporal) read and written back unchanged
    // arrives here as a `Value::Stored` carrier and converts straight back to
    // its original `ModelValue` (ADR-0038) — no base-record comparison needed,
    // and a genuine `SET p = '<string>'` correctly stores a string.
    let mut properties = BTreeMap::new();
    for (name, value) in &node.properties {
        if key_names.iter().any(|k| k == name) {
            continue;
        }
        properties.insert(name.clone(), convert_value(value)?);
    }
    Ok((node_key, NodeRecord::new(secondary, properties)))
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
                key: format_key_tuple(key.key()),
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
        // A read carrier round-trips to its original typed value (ADR-0038):
        // an untouched `Bytes`/temporal property is written back as itself, not
        // retyped to a string. This closes the loss for nodes and edges alike.
        Value::Stored(mv) => mv.clone(),
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
        // ADR-0038 (supersedes ADR-0029/U2): the read adapter carries
        // Bytes/temporal as `Value::Stored`, so a read→modify→write cycle writes
        // the untouched ones straight back as their original type — no base-record
        // re-read. `name` is genuinely changed; `data` (Bytes) and `when`
        // (DateTime) arrive exactly as the adapter produced them.
        let data = ModelValue::Bytes(vec![0xAB, 0xCD, 0xEF]);
        let when = ModelValue::DateTime(DateTime {
            epoch_nanos: 1_600_000_000_000_000_000,
            offset_minutes: 60,
        });
        // The adapter carries deferred types as `Value::Stored` (not a string).
        assert!(matches!(
            crate::exec::adapter::convert_value(&data),
            Value::Stored(_)
        ));
        let node = runtime(&[
            ("id", Value::Int(1)),
            ("name", Value::String("new".into())), // changed
            ("data", crate::exec::adapter::convert_value(&data)), // read back unchanged
            ("when", crate::exec::adapter::convert_value(&when)), // read back unchanged
        ]);

        let (key, record) = node_key_and_record(&node, &catalogue()).expect("persist");
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
        // Setting a deferred property to a genuine string value stores the
        // string, not the old typed value. A user `SET` yields a `Value::String`
        // (never a `Value::Stored`), so this also pins ADR-0038's fix of the
        // ADR-0029 false-positive: assigning a property its own rendered string
        // now correctly stores a string.
        let node = runtime(&[
            ("id", Value::Int(1)),
            ("data", Value::String("changed".into())),
        ]);
        let (_key, record) = node_key_and_record(&node, &catalogue()).expect("persist");
        assert_eq!(
            record.properties().get("data"),
            Some(&ModelValue::String("changed".into()))
        );
    }

    #[test]
    fn a_node_with_no_labels_gets_the_missing_colon_hint() {
        // A bare `(Topic {…})` parses `Topic` as a variable, so the node
        // reaches persist with zero labels: point at the missing colon.
        let node = NodeValue {
            id: EntityId::from_bytes(b"overlay".to_vec()),
            labels: vec![],
            properties: [("id".to_string(), Value::Int(1))].into_iter().collect(),
        };
        let err = node_key_and_record(&node, &catalogue()).expect_err("no identity");
        let msg = err.to_string();
        assert!(msg.contains("this node has no labels"), "{msg}");
        assert!(msg.contains("`(:Topic {…})`"), "{msg}");
        assert!(
            !msg.contains("[]"),
            "must not leak an empty label list: {msg}"
        );
    }

    #[test]
    fn a_labelled_node_with_no_keyed_label_names_the_labels_and_the_fix() {
        // `Ghost` is not in the catalogue (so declares no key): name the
        // labels via the escaped renderer and give the declare-label fix.
        let node = NodeValue {
            id: EntityId::from_bytes(b"overlay".to_vec()),
            labels: vec!["Ghost".to_string()],
            properties: [("id".to_string(), Value::Int(1))].into_iter().collect(),
        };
        let err = node_key_and_record(&node, &catalogue()).expect_err("no identity");
        let msg = err.to_string();
        assert!(
            msg.contains("none of the labels [\"Ghost\"] declares a key"),
            "{msg}"
        );
        assert!(
            msg.contains("acetone declare-label \"Ghost\" --key <property>"),
            "{msg}"
        );
    }

    #[test]
    fn a_created_node_has_no_base_and_converts_directly() {
        let node = runtime(&[("id", Value::Int(1)), ("name", Value::String("x".into()))]);
        let (_key, record) = node_key_and_record(&node, &catalogue()).expect("persist");
        assert_eq!(
            record.properties().get("name"),
            Some(&ModelValue::String("x".into()))
        );
    }
}
