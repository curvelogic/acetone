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

/// A declared index the store-backed seek can serve (single or
/// composite — ADR-0027, acetone-0c7).
struct IndexInfo {
    label: String,
    /// The indexed properties, in declared order.
    properties: Vec<String>,
    /// Each property's declared type, if the schema types it — the
    /// discriminator for whether a string pin is safe to seek (see the
    /// module docs), per component.
    property_types: Vec<Option<PropertyType>>,
}

/// A [`GraphSource`] that reads lazily from a stored [`Snapshot`].
pub struct StoreBackedSource<'s> {
    snapshot: &'s Snapshot<'s>,
    /// label → key property names (to re-expose key values as properties).
    key_names: HashMap<String, Vec<String>>,
    /// index name → its label, ordered properties and per-component types.
    indexes: HashMap<String, IndexInfo>,
    /// The first store read error hit during a query, surfaced by the caller.
    error: Cell<Option<GraphError>>,
}

impl<'s> StoreBackedSource<'s> {
    /// Build over `snapshot`, using `schema` for key-property names and
    /// the seekable declared indexes (single and composite).
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
                let property_types = def
                    .properties()
                    .iter()
                    .map(|property| {
                        label_types
                            .get(def.label())
                            .and_then(|types| types.get(property))
                            .copied()
                    })
                    .collect();
                indexes.insert(
                    name.clone(),
                    IndexInfo {
                        label: def.label().to_owned(),
                        properties: def.properties().to_vec(),
                        property_types,
                    },
                );
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

    /// Probe alternatives for the `component`-th indexed property: the
    /// candidate raw model values whose stored index key the pin could
    /// equal, or `None` to fall back to a scan (the pin cannot be
    /// served exactly).
    fn probe_value(
        &self,
        info: &IndexInfo,
        component: usize,
        value: &Value,
    ) -> Option<Vec<ModelValue>> {
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
            Value::String(s) => match info.property_types.get(component).copied().flatten() {
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

    fn nodes_by_key(&self, label: &str, key_values: &[Value]) -> Option<Vec<NodeValue>> {
        // A primary-key pin is an exact lookup in the nodes map — the
        // cheapest seek there is, and the one the shipped read path was
        // missing entirely (Phase 9 security review finding 7).
        //
        // Candidate-superset semantics as everywhere else: probe both
        // numeric encodings (3 == 3.0 in openCypher but they encode
        // differently), and a probe set we cannot form means "cannot
        // serve" — scan — never "definitively absent".
        let mut per_component: Vec<Vec<ModelValue>> = Vec::with_capacity(key_values.len());
        for value in key_values {
            let alternatives = match value {
                // Null/NaN can never be a key value (keys are non-null),
                // so nothing matches — but say "cannot serve" rather than
                // asserting absence.
                Value::Null => return None,
                Value::Float(f) if f.is_nan() => return None,
                Value::Int(n) => vec![ModelValue::Int(*n), ModelValue::Float(*n as f64)],
                Value::Float(f) => {
                    // An integral float >= 2^53 has a non-unique i64
                    // preimage; a single probe would under-select.
                    if f.fract() == 0.0 && f.abs() >= 9_007_199_254_740_992.0 {
                        return None;
                    }
                    let mut alts = vec![ModelValue::Float(*f)];
                    if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 {
                        alts.push(ModelValue::Int(*f as i64));
                    }
                    alts
                }
                // A STRING pin cannot be served: a Bytes/temporal key
                // value compares equal to its string *rendering* at
                // runtime (ADR-0038 carriers decay under eq3), and the
                // stored encodings differ, so a probe on the string
                // encoding alone would MISS such a node — under-selection,
                // which candidate-superset semantics forbid. `probe_value`
                // guards this for indexes via the declared type; keys have
                // no such guard here, so decline and let the scan answer
                // (PR #219 review blocker 3 — a wrong-answer regression).
                Value::String(_) => return None,
                // A carrier pin has the same hazard from the other side.
                Value::Stored(_) => return None,
                other => vec![crate::exec::adapter::model_value_of(other)?],
            };
            per_component.push(alternatives);
        }
        // `product()` wraps on overflow in release, so a key of >=64
        // numeric components made the cap vacuous and the tuple loop
        // allocate unboundedly (PR #219 review finding 4).
        let mut combinations: usize = 1;
        for alternatives in &per_component {
            combinations = combinations.checked_mul(alternatives.len())?;
            if combinations > 16 {
                return None;
            }
        }
        let mut tuples: Vec<Vec<ModelValue>> = vec![Vec::new()];
        for alternatives in &per_component {
            let mut next = Vec::with_capacity(tuples.len() * alternatives.len());
            for prefix in &tuples {
                for alt in alternatives {
                    let mut tuple = prefix.clone();
                    tuple.push(alt.clone());
                    next.push(tuple);
                }
            }
            tuples = next;
        }
        let mut out = Vec::new();
        let mut served = false;
        for tuple in tuples {
            let Ok(key) = acetone_model::graph_keys::NodeKey::new(label, tuple) else {
                continue;
            };
            match self.snapshot.get_node(&key) {
                Ok(Some(record)) => {
                    served = true;
                    out.push(node_value(&key, &record, &self.key_names));
                }
                // A well-formed probe that finds nothing is a real answer
                // for an exact key, but stay conservative and let the
                // scan confirm rather than assert absence here.
                Ok(None) => {}
                Err(e) => return self.fail(e, None),
            }
        }
        if served { Some(out) } else { None }
    }

    fn nodes_by_index(
        &self,
        index_name: &str,
        properties: &[String],
        values: &[&Value],
    ) -> Option<Vec<NodeValue>> {
        let info = self.indexes.get(index_name)?;
        // The hint may have been bound against another version's catalogue
        // (AT clauses); a same-named index over different properties must
        // not serve it (PR #206 review finding 4), and the value tuple
        // must match the declared arity.
        if info.properties.as_slice() != properties || values.len() != properties.len() {
            return None;
        }
        // Per-component probe alternatives, then their bounded cartesian
        // (mirrors adapter.rs::cartesian_probes over byte encodings —
        // keep caps and bail rules aligned): a composite entry's key is
        // the ordered value tuple (ADR-0027).
        let mut per_component: Vec<Vec<ModelValue>> = Vec::with_capacity(values.len());
        for (component, value) in values.iter().enumerate() {
            let probes = self.probe_value(info, component, value)?;
            if probes.is_empty() {
                // A null/NaN component matches nothing (null-blind index).
                return Some(Vec::new());
            }
            per_component.push(probes);
        }
        let combinations: usize = per_component.iter().map(Vec::len).product();
        if combinations > 16 {
            return None;
        }
        let mut tuples: Vec<Vec<ModelValue>> = vec![Vec::new()];
        for alternatives in &per_component {
            let mut next = Vec::with_capacity(tuples.len() * alternatives.len());
            for prefix in &tuples {
                for alt in alternatives {
                    let mut tuple = prefix.clone();
                    tuple.push(alt.clone());
                    next.push(tuple);
                }
            }
            tuples = next;
        }
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        let mut out = Vec::new();
        for tuple in tuples {
            let prefix = match index_value_prefix(&info.label, properties, &tuple) {
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
