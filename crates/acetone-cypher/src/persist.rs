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
    #[error(
        "the key property {property:?} of {label:?} has a {type_name} value, which cannot form \
         a node key: key properties must be boolean, integer, float or string (bytes and \
         temporal values are not identity-round-trippable through queries)"
    )]
    KeyValueType {
        /// The primary label whose key the value was destined for.
        label: String,
        /// The offending key property name.
        property: String,
        /// The rejected value's type, rendered for the message.
        type_name: &'static str,
    },
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
        let converted = convert_value(value)?;
        // Defensive guard (acetone-7vw): the deferred types (Bytes and the
        // temporals) must not form node identity. They have no native runtime
        // representation — they travel as `Value::Stored` carriers whose
        // query semantics are string renderings (ADR-0038) — so an identity
        // comparison on such a key (e.g. a MERGE match-or-create) would
        // compare renderings while the persisted key stays typed: a mismatch
        // mints a duplicate node and orphans its edges. Unreachable from
        // today's Cypher/CLI surface, which cannot produce these values in
        // key position; rejected cleanly rather than trusted.
        if let Some(type_name) = deferred_type_name(&converted) {
            return Err(PersistError::KeyValueType {
                label: primary.clone(),
                property: name.clone(),
                type_name,
            });
        }
        key_values.push(converted);
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

/// The rendered type name of a deferred (non-identity-round-trippable)
/// model value — Bytes and the four temporals — or `None` for the types
/// that may form node keys. Lists and nulls are not key material either,
/// but [`NodeKey::new`] already rejects those with its own typed errors.
fn deferred_type_name(value: &ModelValue) -> Option<&'static str> {
    match value {
        ModelValue::Bytes(_) => Some("bytes"),
        ModelValue::Date(_) => Some("date"),
        ModelValue::Time(_) => Some("time"),
        ModelValue::DateTime(_) => Some("datetime"),
        ModelValue::Duration(_) => Some("duration"),
        ModelValue::Null
        | ModelValue::Bool(_)
        | ModelValue::Int(_)
        | ModelValue::Float(_)
        | ModelValue::String(_)
        | ModelValue::List(_) => None,
    }
}

/// Convert a runtime value to a storable model value. Maps, nodes,
/// relationships and paths are not storable property values, and neither is
/// anything nested past [`crate::exec::adapter::MAX_VALUE_DEPTH`]
/// (acetone-5xp): a runtime value can nest arbitrarily deep (e.g. a reduce
/// that wraps a list each step), so the walk carries a depth counter and
/// surfaces a clean error instead of recursing without bound. The model's
/// encoder remains the semantic authority for storable depth (it rejects
/// anything past its own, lower cap of 128); this guard only bounds the
/// recursion getting there.
fn convert_value(value: &Value) -> Result<ModelValue, PersistError> {
    convert_value_at(value, 0)
}

fn convert_value_at(value: &Value, depth: usize) -> Result<ModelValue, PersistError> {
    if depth >= crate::exec::adapter::MAX_VALUE_DEPTH {
        return Err(PersistError::Value("list nested past the depth limit"));
    }
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
                .map(|item| convert_value_at(item, depth + 1))
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

    #[test]
    #[test]
    fn deferred_typed_key_values_are_rejected_with_a_typed_error() {
        // acetone-7vw: Bytes and temporal values must not form node identity —
        // their query semantics are string renderings (ADR-0038 carriers), so
        // identity comparison and persisted identity would disagree.
        let cases: Vec<(ModelValue, &str)> = vec![
            (ModelValue::Bytes(vec![0xAB, 0xCD]), "bytes"),
            (ModelValue::Date(Date { days: 20_000 }), "date"),
            (ModelValue::Time(Time { nanos: 1 }), "time"),
            (
                ModelValue::DateTime(DateTime {
                    epoch_nanos: 1_600_000_000_000_000_000,
                    offset_minutes: 60,
                }),
                "datetime",
            ),
            (
                ModelValue::Duration(Duration {
                    months: 1,
                    days: 2,
                    nanos: 3,
                }),
                "duration",
            ),
        ];
        for (value, expected_type) in cases {
            let node = runtime(&[("id", crate::exec::adapter::convert_value(&value))]);
            let err = node_key_and_record(&node, &catalogue())
                .expect_err("a deferred-typed key value must be rejected");
            match &err {
                PersistError::KeyValueType {
                    label,
                    property,
                    type_name,
                } => {
                    assert_eq!(label, "L");
                    assert_eq!(property, "id");
                    assert_eq!(*type_name, expected_type);
                }
                other => panic!("expected KeyValueType for {expected_type}, got {other:?}"),
            }
            assert!(
                err.to_string().contains(expected_type),
                "message must name the type: {err}"
            );
        }
    }

    #[test]
    fn deferred_types_stay_storable_as_non_key_properties() {
        // The guard is key-position only: the ADR-0038 carrier round-trip for
        // ordinary properties is untouched.
        let when = ModelValue::DateTime(DateTime {
            epoch_nanos: 1,
            offset_minutes: 0,
        });
        let node = runtime(&[
            ("id", Value::Int(1)),
            ("when", crate::exec::adapter::convert_value(&when)),
        ]);
        let (_key, record) = node_key_and_record(&node, &catalogue()).expect("persist");
        assert_eq!(record.properties().get("when"), Some(&when));
    }

    // --- key-type guard properties (acetone-7vw) ---------------------------

    use acetone_model::{Date, Duration, Time};
    use proptest::prelude::*;

    /// Key values every shipped surface can produce: the identity-
    /// round-trippable scalars (spec §2 key material).
    fn round_trippable_key_value() -> impl Strategy<Value = ModelValue> {
        prop_oneof![
            any::<bool>().prop_map(ModelValue::Bool),
            any::<i64>().prop_map(ModelValue::Int),
            any::<f64>().prop_map(|x| ModelValue::Float(if x.is_nan() { 0.5 } else { x })),
            ".{1,12}".prop_map(ModelValue::String),
        ]
    }

    /// The deferred types: valid stored values, but not key material.
    fn deferred_key_value() -> impl Strategy<Value = ModelValue> {
        prop_oneof![
            proptest::collection::vec(any::<u8>(), 0..8).prop_map(ModelValue::Bytes),
            any::<i64>().prop_map(|d| ModelValue::Date(Date { days: d })),
            (0..86_400_000_000_000u64).prop_map(|n| ModelValue::Time(Time { nanos: n })),
            (any::<i64>(), -1080i16..=1080).prop_map(|(n, o)| {
                ModelValue::DateTime(DateTime {
                    epoch_nanos: n,
                    offset_minutes: o,
                })
            }),
            (any::<i64>(), any::<i64>(), any::<i64>()).prop_map(|(m, d, n)| {
                ModelValue::Duration(Duration {
                    months: m,
                    days: d,
                    nanos: n,
                })
            }),
        ]
    }

    proptest! {
        /// Every round-trippable key value persists, and the derived key is
        /// exactly the declared value — identity survives the runtime trip.
        #[test]
        fn round_trippable_key_values_persist_with_identity_intact(
            v in round_trippable_key_value()
        ) {
            let node = runtime(&[("id", crate::exec::adapter::convert_value(&v))]);
            let (key, _record) = node_key_and_record(&node, &catalogue())
                .map_err(|e| TestCaseError::fail(format!("must persist: {e}")))?;
            let expected = NodeKey::new("L", vec![v.clone()])
                .map_err(|e| TestCaseError::fail(format!("valid key: {e}")))?;
            prop_assert_eq!(&key, &expected);
            // And the storage encoding round-trips the same identity.
            let bytes = key.encode()
                .map_err(|e| TestCaseError::fail(format!("encodable: {e}")))?;
            prop_assert_eq!(NodeKey::decode(&bytes).expect("decodable"), expected);
        }

        /// Every deferred-typed key value is rejected with the typed error —
        /// never persisted, never a panic, never a silently retyped key.
        #[test]
        fn deferred_typed_key_values_always_error_cleanly(v in deferred_key_value()) {
            let node = runtime(&[("id", crate::exec::adapter::convert_value(&v))]);
            let result = node_key_and_record(&node, &catalogue());
            prop_assert!(
                matches!(result, Err(PersistError::KeyValueType { .. })),
                "expected KeyValueType for {:?}, got {:?}", v, result
            );
        }
    }

    #[test]
    fn an_over_deep_runtime_list_fails_persist_with_a_clean_error() {
        // acetone-5xp: a runtime value can nest arbitrarily deep (a reduce
        // wrapping a list each step), so the write-path conversion carries a
        // depth counter and surfaces a typed error instead of recursing the
        // stack away. A modestly nested list still persists untouched.
        let shallow = Value::List(vec![Value::List(vec![Value::Int(1)])]);
        assert!(convert_value(&shallow).is_ok());

        let mut deep = Value::Int(1);
        for _ in 0..50_000 {
            deep = Value::List(vec![deep]);
        }
        let err = convert_value(&deep).expect_err("an over-deep value must not persist");
        assert!(
            err.to_string().contains("depth limit"),
            "unexpected error: {err}"
        );
        // Tear the fixture down iteratively so its drop glue cannot recurse
        // off the stack either.
        let mut stack = vec![deep];
        while let Some(value) = stack.pop() {
            if let Value::List(items) = value {
                stack.extend(items);
            }
        }
    }
}
