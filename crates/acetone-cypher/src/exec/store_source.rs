//! A lazy, store-backed [`GraphSource`] (ADR-0040, `acetone-cbl.11`).
//!
//! [`GraphSnapshot`](crate::exec::GraphSnapshot) materialises a whole version up
//! front. This source instead reads only what each query touches, straight from
//! the stored prolly maps of an [`acetone_graph::repo::Snapshot`]:
//!
//! - an `IndexSeek` reads only the matching `idx/<name>` entries and fetches
//!   only those node records (the scalability win the secondary index exists
//!   for);
//! - `expand` reads only a node's incident edges (`edges_fwd`/`edges_rev`);
//! - a full label scan still materialises (`all_nodes`) — inherently
//!   O(version) — but a seek/expand-anchored query never reaches it.
//!
//! ## Two correctness hazards, handled here
//!
//! **Lazy reads can fail mid-query.** The [`GraphSource`] methods are infallible
//! (they were designed for a pre-materialised snapshot). A store read that fails
//! here is recorded in [`StoreBackedSource::error`] and returned as empty/None;
//! the caller drains it with [`StoreBackedSource::take_error`] after execution
//! and turns it into a query error, so a corrupt read surfaces rather than
//! silently dropping rows.
//!
//! **Raw stored keys vs. rendered scan matches.** The stored index keys the
//! *raw typed* value, but a scan matches a `Bytes`/temporal property by its
//! *string rendering* (the [`Value::Stored`](crate::exec::value::Value::Stored)
//! carrier decays to a string under `eq3`). A raw-keyed probe would miss those,
//! under-selecting. So [`Self::nodes_by_index`] only serves a pin when a raw
//! probe cannot miss: numeric and boolean pins always (they never cross-type
//! match a rendering), a string pin only when the indexed property's declared
//! type is a non-deferred scalar; otherwise it falls back to a scan (`None`).

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use acetone_graph::GraphError;
use acetone_graph::repo::Snapshot;
use acetone_model::Value as ModelValue;
use acetone_model::graph_keys::{NodeKey, index_value_prefix};
use acetone_model::schema::{PropertyType, SchemaEntry};

use crate::ast::Direction;
use crate::exec::adapter::{node_value, rel_value};
use crate::exec::source::GraphSource;
use crate::exec::value::{EntityId, NodeValue, RelValue, Value};

/// A single-property declared index the store-backed seek can serve.
struct IndexInfo {
    label: String,
    property: String,
    /// The property's declared type, if the schema types it — the discriminator
    /// for whether a string pin is safe to seek (see the module docs).
    property_type: Option<PropertyType>,
}

/// A [`GraphSource`] that reads lazily from a stored [`Snapshot`].
pub struct StoreBackedSource<'s> {
    snapshot: &'s Snapshot<'s>,
    /// label → key property names (to re-expose key values as properties).
    key_names: HashMap<String, Vec<String>>,
    /// index name → `(label, property, type)` for single-property indexes.
    indexes: HashMap<String, IndexInfo>,
    /// The first store read error hit during a query, surfaced by the caller.
    error: Cell<Option<GraphError>>,
}

impl<'s> StoreBackedSource<'s> {
    /// Build over `snapshot`, using `schema` for key-property names and the
    /// seekable single-property indexes.
    pub fn new(snapshot: &'s Snapshot<'s>, schema: &[SchemaEntry]) -> Self {
        let mut key_names: HashMap<String, Vec<String>> = HashMap::new();
        let mut label_types: HashMap<String, BTreeMap<String, PropertyType>> = HashMap::new();
        for entry in schema {
            if let SchemaEntry::Label { name, def } = entry {
                key_names.insert(name.clone(), def.key().to_vec());
                label_types.insert(name.clone(), def.types().clone());
            }
        }
        let mut indexes: HashMap<String, IndexInfo> = HashMap::new();
        for entry in schema {
            if let SchemaEntry::Index { name, def } = entry {
                // Only single-property indexes drive the seek (ADR-0022);
                // composite indexes scan-and-filter.
                if let [property] = def.properties() {
                    let property_type = label_types
                        .get(def.label())
                        .and_then(|types| types.get(property))
                        .copied();
                    indexes.insert(
                        name.clone(),
                        IndexInfo {
                            label: def.label().to_owned(),
                            property: property.clone(),
                            property_type,
                        },
                    );
                }
            }
        }
        StoreBackedSource {
            snapshot,
            key_names,
            indexes,
            error: Cell::new(None),
        }
    }

    /// Take the first store read error hit during a query, if any. The caller
    /// runs this after execution: a lazy read cannot return its error through
    /// the infallible [`GraphSource`] trait, so it is recorded and drained here.
    pub fn take_error(&self) -> Option<GraphError> {
        self.error.take()
    }

    /// Record the first error and yield the fallback the trait method returns.
    /// A later error is a downstream symptom of the first, so the first is kept.
    fn fail<T>(&self, error: GraphError, fallback: T) -> T {
        let first = self.error.take().or(Some(error));
        self.error.set(first);
        fallback
    }

    /// Fetch and build one node by its stored key, recording any read error.
    fn node_from_key(&self, key: &NodeKey) -> Option<NodeValue> {
        match self.snapshot.get_node(key) {
            Ok(Some(record)) => Some(node_value(key, &record, &self.key_names)),
            Ok(None) => None,
            Err(e) => self.fail(e, None),
        }
    }

    /// Decode an entity id back to its stored node key. The id is exactly the
    /// `nodes`-map key encoding ([`NodeKey::encode`]), so this round-trips.
    fn key_of(&self, id: &EntityId) -> Option<NodeKey> {
        NodeKey::decode(id.0.as_ref()).ok()
    }

    /// The candidate raw model values whose stored index key a pin could equal,
    /// or `None` to fall back to a scan (the pin cannot be served exactly).
    fn probe_values(&self, info: &IndexInfo, value: &Value) -> Option<Vec<ModelValue>> {
        match value {
            // A null or NaN pin selects nothing (indexes are null/NaN-blind).
            Value::Null => Some(Vec::new()),
            Value::Float(f) if f.is_nan() => Some(Vec::new()),
            // A list pin needs element-wise equality no byte bucket serves.
            Value::List(_) => None,
            // A carrier never originates in a query pin; be safe and scan.
            Value::Stored(_) => None,
            // A numeric pin is always safe: it can never cross-type match a
            // Bytes/temporal *rendering* (a hex/debug string), so no raw entry
            // is missed. Probe BOTH numeric encodings (3 = 3.0).
            Value::Int(n) => Some(vec![ModelValue::Int(*n), ModelValue::Float(*n as f64)]),
            Value::Float(f) => {
                // An integer-valued float ≥ 2^53 has a non-unique i64 preimage,
                // so the single `f as i64` probe would under-select — scan.
                if f.fract() == 0.0 && f.abs() >= 9_007_199_254_740_992.0 {
                    return None;
                }
                let mut values = vec![ModelValue::Float(*f)];
                if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                    values.push(ModelValue::Int(*f as i64));
                }
                Some(values)
            }
            Value::Bool(b) => Some(vec![ModelValue::Bool(*b)]),
            // A string pin could equal a Bytes/temporal value's rendering, which
            // is keyed raw — so a raw probe would miss it. Safe only when the
            // property's declared type rules out a deferred value.
            Value::String(s) => match info.property_type {
                Some(PropertyType::String)
                | Some(PropertyType::Int)
                | Some(PropertyType::Float)
                | Some(PropertyType::Bool) => Some(vec![ModelValue::String(s.clone())]),
                _ => None,
            },
            // Non-storable kinds never index.
            Value::Map(_) | Value::Node(_) | Value::Relationship(_) | Value::Path(_) => {
                Some(Vec::new())
            }
        }
    }
}

impl GraphSource for StoreBackedSource<'_> {
    fn all_nodes(&self) -> Vec<NodeValue> {
        match self.snapshot.nodes() {
            Ok(nodes) => nodes
                .iter()
                .map(|(key, record)| node_value(key, record, &self.key_names))
                .collect(),
            Err(e) => self.fail(e, Vec::new()),
        }
    }

    fn nodes_by_index(&self, index_name: &str, value: &Value) -> Option<Vec<NodeValue>> {
        let info = self.indexes.get(index_name)?;
        let probes = self.probe_values(info, value)?;
        let properties = std::slice::from_ref(&info.property);
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let mut out = Vec::new();
        for model in probes {
            let prefix =
                match index_value_prefix(&info.label, properties, std::slice::from_ref(&model)) {
                    Ok(prefix) => prefix,
                    // A value that cannot encode (e.g. a NaN nested somewhere)
                    // contributes no entries — it indexes nothing.
                    Err(_) => continue,
                };
            match self.snapshot.index_scan(index_name, &prefix) {
                // Index map absent though the schema declares it: fall back.
                Ok(None) => return None,
                Ok(Some(keys)) => {
                    for key in keys {
                        let Ok(encoded) = key.encode() else { continue };
                        if seen.insert(encoded)
                            && let Some(node) = self.node_from_key(&key)
                        {
                            out.push(node);
                        }
                    }
                }
                Err(e) => return self.fail(e, None),
            }
        }
        Some(out)
    }

    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        let Some(key) = self.key_of(node) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let type_ok = |rtype: &str| types.is_empty() || types.iter().any(|t| t == rtype);

        // Out-edges first (matching GraphSnapshot's order), then in-edges.
        if matches!(direction, Direction::Out | Direction::Undirected) {
            match self.snapshot.out_edges(&key) {
                Ok(edges) => {
                    for (edge, record) in edges {
                        if !type_ok(edge.rtype()) {
                            continue;
                        }
                        if let Some(neighbour) = self.node_from_key(edge.dst()) {
                            out.push((rel_value(&edge, &record), neighbour));
                        }
                    }
                }
                Err(e) => return self.fail(e, Vec::new()),
            }
        }
        if matches!(direction, Direction::In | Direction::Undirected) {
            match self.snapshot.in_edges(&key) {
                Ok(edges) => {
                    for (edge, record) in edges {
                        // A self-loop is already emitted by the out pass; skip
                        // its second sighting under Undirected.
                        if direction == Direction::Undirected && edge.src() == &key {
                            continue;
                        }
                        if !type_ok(edge.rtype()) {
                            continue;
                        }
                        if let Some(neighbour) = self.node_from_key(edge.src()) {
                            out.push((rel_value(&edge, &record), neighbour));
                        }
                    }
                }
                Err(e) => return self.fail(e, Vec::new()),
            }
        }
        out
    }

    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        let key = self.key_of(id)?;
        self.node_from_key(&key)
    }
}
