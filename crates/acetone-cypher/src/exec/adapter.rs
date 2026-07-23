//! Bridges stored graph records (acetone-graph / acetone-model) to the
//! executor's [`GraphSource`]. Builds a materialised in-memory snapshot
//! once — at workbench scale (spec §1) that is cheap, and it decouples
//! execution from the storage layer's lifetimes. A streaming provider is
//! a later optimisation.
//!
//! `AT <ref>` whole-query time travel is served by the caller choosing
//! which stored version's records to hand in (the CLI reads at a resolved
//! ref); clause-group `AT` inside a query stays with acetone-yzc.7.

use std::collections::{BTreeMap, HashMap};

use acetone_model::Value as ModelValue;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::SchemaEntry;

use crate::ast::Direction;
use crate::bind::Catalogue;
use crate::exec::source::GraphSource;
use crate::exec::value::{EntityId, NodeValue, RelValue, Value};

/// A materialised snapshot of a stored graph version, ready to execute
/// against. Indexed at construction — node lookup, label scan and edge
/// expansion must be sub-linear or realistic graphs are unqueryable
/// (a linear scan per expand is O(nodes·edges) over a whole MATCH).
#[derive(Debug, Default)]
pub struct GraphSnapshot {
    nodes: Vec<NodeValue>,
    rels: Vec<RelValue>,
    /// Node id → index into `nodes` (point lookup / neighbour resolve).
    by_id: HashMap<EntityId, usize>,
    /// Label → node indices (LabelScan; the empty-label "all" case reads
    /// `nodes` directly).
    by_label: HashMap<String, Vec<usize>>,
    /// Declared index name → encoded property value → node indices
    /// (IndexSeek). Built for the schema's declared indexes, keyed by the
    /// memcomparable value encoding so lookups match the stored `idx/<name>`
    /// map's selection exactly (null/NaN-blind).
    by_index: HashMap<String, HashMap<Vec<u8>, Vec<usize>>>,
    /// Node id → indices into `rels` of edges leaving it (ExpandOut).
    out_edges: HashMap<EntityId, Vec<usize>>,
    /// Node id → indices into `rels` of edges entering it (ExpandIn).
    in_edges: HashMap<EntityId, Vec<usize>>,
}

impl GraphSnapshot {
    /// Build from a version's node and edge records (e.g. from a
    /// `Repository`/`Snapshot`'s `nodes()` and `edges()`), constructing
    /// the id/label/adjacency indexes.
    ///
    /// Key properties are not exposed (there is no schema to name them) —
    /// suitable for schema-free graphs (the TCK backend, tests). For a
    /// stored graph with a declared schema, use
    /// [`Self::from_records_with_schema`] so key values become queryable.
    pub fn from_records(nodes: &[(NodeKey, NodeRecord)], edges: &[(EdgeKey, EdgeRecord)]) -> Self {
        Self::build(nodes, edges, &HashMap::new(), &[])
    }

    /// Build with the schema's key-property names, so a node's key values
    /// are re-exposed as queryable properties — `MATCH (h:Host {hostname:
    /// 'web-01'})` and `RETURN h.hostname` work. A node's key IS part of
    /// its data (spec §2/§3); the stored record holds only the non-key
    /// properties, so the key names come from the schema.
    pub fn from_records_with_schema(
        nodes: &[(NodeKey, NodeRecord)],
        edges: &[(EdgeKey, EdgeRecord)],
        schema: &[SchemaEntry],
    ) -> Self {
        let mut key_names: HashMap<String, Vec<String>> = HashMap::new();
        // Only single-property indexes drive the in-memory seek (ADR-0022);
        // composite indexes are maintained and fsck-verified but not yet
        // seek-accelerated (a tracked follow-up), so a composite pin scans.
        let mut index_defs: Vec<(String, String, String)> = Vec::new();
        for entry in schema {
            match entry {
                SchemaEntry::Label { name, def } => {
                    key_names.insert(name.clone(), def.key().to_vec());
                }
                SchemaEntry::Index { name, def } => {
                    if let [property] = def.properties() {
                        index_defs.push((name.clone(), def.label().to_owned(), property.clone()));
                    }
                }
                SchemaEntry::RelType { .. } => {}
            }
        }
        Self::build(nodes, edges, &key_names, &index_defs)
    }

    fn build(
        nodes: &[(NodeKey, NodeRecord)],
        edges: &[(EdgeKey, EdgeRecord)],
        key_names: &HashMap<String, Vec<String>>,
        index_defs: &[(String, String, String)],
    ) -> Self {
        let node_values: Vec<NodeValue> = nodes
            .iter()
            .map(|(key, record)| node_value(key, record, key_names))
            .collect();
        let rel_values: Vec<RelValue> = edges
            .iter()
            .map(|(key, record)| rel_value(key, record))
            .collect();

        let mut by_id = HashMap::with_capacity(node_values.len());
        let mut by_label: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, node) in node_values.iter().enumerate() {
            by_id.insert(node.id.clone(), index);
            for label in &node.labels {
                by_label.entry(label.clone()).or_default().push(index);
            }
        }
        let mut out_edges: HashMap<EntityId, Vec<usize>> = HashMap::new();
        let mut in_edges: HashMap<EntityId, Vec<usize>> = HashMap::new();
        for (index, rel) in rel_values.iter().enumerate() {
            out_edges.entry(rel.start.clone()).or_default().push(index);
            in_edges.entry(rel.end.clone()).or_default().push(index);
        }

        // Declared-index value maps (IndexSeek). Built from the *runtime*
        // node values — the same representation `node_satisfies` filters
        // against — so the seek and the filter agree on what a property is.
        // (This matters for stored `Bytes`/temporal values, which the runtime
        // renders to a string; keying the raw typed value here would let a
        // string-pinned seek miss them.) null/NaN-blind.
        let mut by_index: HashMap<String, HashMap<Vec<u8>, Vec<usize>>> = HashMap::new();
        for (name, label, property) in index_defs {
            let map = by_index.entry(name.clone()).or_default();
            for (index, node) in node_values.iter().enumerate() {
                if let Some(bytes) = index_value_bytes(node, label, property) {
                    map.entry(bytes).or_default().push(index);
                }
            }
        }

        GraphSnapshot {
            nodes: node_values,
            rels: rel_values,
            by_id,
            by_label,
            by_index,
            out_edges,
            in_edges,
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn rel_count(&self) -> usize {
        self.rels.len()
    }

    /// Resolve a rel index's neighbour in `direction` from `node`, if this
    /// rel is incident the right way and its type matches.
    fn incident(
        &self,
        rel_index: usize,
        node: &EntityId,
        types: &[String],
    ) -> Option<(RelValue, NodeValue)> {
        let rel = &self.rels[rel_index];
        if !types.is_empty() && !types.contains(&rel.rel_type) {
            return None;
        }
        let neighbour_id = if rel.start == *node {
            &rel.end
        } else if rel.end == *node {
            &rel.start
        } else {
            return None;
        };
        let neighbour = self
            .by_id
            .get(neighbour_id)
            .map(|&i| self.nodes[i].clone())?;
        Some((rel.clone(), neighbour))
    }
}

/// Build a binder catalogue from a version's schema entries.
pub fn catalogue_from_schema(entries: Vec<SchemaEntry>) -> Catalogue {
    Catalogue::from_entries(entries)
}

/// Stable node identity: the memcomparable logical key bytes. Distinct
/// nodes have distinct keys (identity is `(label, key tuple)`, spec §3).
fn node_entity_id(key: &NodeKey) -> EntityId {
    let logical = key.to_value();
    EntityId::from_bytes(render_key_bytes(&logical))
}

/// The schema's key-property names, keyed by label — the map
/// [`virtual_diff_node`] and [`node_value`] use to re-expose key values as
/// queryable properties. Build it **once per side** and pass it to
/// `virtual_diff_node` for every row: rebuilding it per node would cost
/// O(rows × schema) over a diff (acetone-v8g).
pub fn key_names_from_schema(schema: &[SchemaEntry]) -> HashMap<String, Vec<String>> {
    let mut key_names: HashMap<String, Vec<String>> = HashMap::new();
    for entry in schema {
        if let SchemaEntry::Label { name, def } = entry {
            key_names.insert(name.clone(), def.key().to_vec());
        }
    }
    key_names
}

/// Build a runtime node value for the diff virtual graph (acetone-14c.1):
/// the stored `(key, record)` rendered as a node — key properties re-exposed
/// under their schema-declared names — with a virtual change label
/// (`_Added`/`_Removed`/`_Modified`) prepended to its label set, so a query
/// can select it with `node:_Added`. `key_names` is the
/// [`key_names_from_schema`] map of the version the record belongs to (its
/// `to` version for added/modified, `from` for removed); an empty map (a
/// schemaless version) leaves key properties un-exposed but the node intact.
pub fn virtual_diff_node(
    key: &NodeKey,
    record: &NodeRecord,
    key_names: &HashMap<String, Vec<String>>,
    change_label: &str,
) -> NodeValue {
    let mut node = node_value(key, record, key_names);
    let mut labels = Vec::with_capacity(node.labels.len() + 1);
    labels.push(change_label.to_string());
    labels.append(&mut node.labels);
    node.labels = labels;
    node
}

pub(crate) fn node_value(
    key: &NodeKey,
    record: &NodeRecord,
    key_names: &HashMap<String, Vec<String>>,
) -> NodeValue {
    let mut labels = Vec::with_capacity(1 + record.secondary_labels().len());
    labels.push(key.label().to_string());
    labels.extend(record.secondary_labels().iter().cloned());

    let mut properties = convert_map(record.properties());
    // Re-expose the key values under their schema-declared property names
    // so they are filterable and returnable (the record stores only the
    // non-key properties). Non-key properties win a name collision — they
    // should never disagree, but the record is authoritative for those.
    if let Some(names) = key_names.get(key.label()) {
        for (name, value) in names.iter().zip(key.key()) {
            properties
                .entry(name.clone())
                .or_insert_with(|| convert_value(value));
        }
    }

    NodeValue {
        id: node_entity_id(key),
        labels,
        properties,
    }
}

/// Stable relationship identity: the memcomparable forward-key bytes
/// `(src, type, dst, disc)`. Distinct edges have distinct keys (relationship
/// identity is `(src, type, dst, discriminator)`, spec §2), so this is stable
/// across snapshots and round-trips back to the [`EdgeKey`] — unlike the
/// former positional `e{index}` (acetone-rid, ADR-0037). Mirrors
/// [`node_entity_id`]. The encoding succeeds for any valid stored key; the
/// `Debug` fallback matches [`render_key_bytes`]'s defensive shape.
fn rel_entity_id(key: &EdgeKey) -> EntityId {
    EntityId::from_bytes(
        key.encode_fwd()
            .unwrap_or_else(|_| format!("{key:?}").into_bytes()),
    )
}

pub(crate) fn rel_value(key: &EdgeKey, record: &EdgeRecord) -> RelValue {
    RelValue {
        id: rel_entity_id(key),
        rel_type: key.rtype().to_string(),
        start: node_entity_id(key.src()),
        end: node_entity_id(key.dst()),
        properties: convert_map(record.properties()),
    }
}

fn convert_map(properties: &BTreeMap<String, ModelValue>) -> BTreeMap<String, Value> {
    properties
        .iter()
        .map(|(key, value)| (key.clone(), convert_value(value)))
        .collect()
}

/// Convert a stored value to a runtime value. The v0.1 read subset
/// (spec §5.1) defers temporal and byte types: the runtime `Value` has no
/// native `Bytes`/temporal variant, so those are wrapped in a
/// [`Value::Stored`] carrier (ADR-0038) rather than made unqueryable.
///
/// The carrier presents as its string rendering ([`render_stored`]) in every
/// query semantic, so property access, comparison and display are unchanged;
/// its sole purpose is that the write path ([`persist`](crate::persist)) can
/// recover the original typed [`ModelValue`], closing the read→write retyping
/// loss for both nodes and edges (this supersedes the ADR-0029 node-only
/// heuristic).
pub(crate) fn convert_value(value: &ModelValue) -> Value {
    convert_value_at(value, 0)
}

/// Maximum nesting depth the stored-value walks will recurse into
/// (acetone-5xp, defence in depth). Stored values are bounded at
/// [`acetone_model::values::MAX_DEPTH`] (128) by both the CBOR encoder and
/// decoder, so no decodable value can reach this cap — the guard exists so
/// that a hostile or corrupt value that somehow bypassed those caps meets a
/// defined, non-panicking bound here instead of unbounded recursion (a stack
/// smash) on the read path.
pub(crate) const MAX_VALUE_DEPTH: usize = 256;
const _: () = assert!(
    MAX_VALUE_DEPTH > acetone_model::values::MAX_DEPTH,
    "the defence-in-depth cap must sit above the model's own encode/decode cap, \
     or legitimate stored values would be degraded"
);

fn convert_value_at(value: &ModelValue, depth: usize) -> Value {
    if depth >= MAX_VALUE_DEPTH {
        // Unreachable for any value that passed the model's encode/decode
        // depth caps (see MAX_VALUE_DEPTH). This walk cannot error — it feeds
        // the infallible, 0.2-frozen `GraphSource` surface — so beyond-cap
        // nesting degrades to null rather than recursing on.
        return Value::Null;
    }
    match value {
        ModelValue::Null => Value::Null,
        ModelValue::Bool(b) => Value::Bool(*b),
        ModelValue::Int(n) => Value::Int(*n),
        ModelValue::Float(x) => Value::Float(*x),
        ModelValue::String(s) => Value::String(s.clone()),
        ModelValue::List(items) => Value::List(
            items
                .iter()
                .map(|item| convert_value_at(item, depth + 1))
                .collect(),
        ),
        // Deferred domain types (`Bytes` and the four temporals): carried
        // verbatim so the round-trip is lossless. Exhaustive by design — a new
        // `ModelValue` variant must make a deliberate carry-or-model choice.
        ModelValue::Bytes(_)
        | ModelValue::Date(_)
        | ModelValue::Time(_)
        | ModelValue::DateTime(_)
        | ModelValue::Duration(_) => Value::Stored(value.clone()),
    }
}

/// A deterministic byte rendering of a node's logical key for identity.
fn render_key_bytes(value: &ModelValue) -> Vec<u8> {
    // Reuse the model's memcomparable encoding when it succeeds (it does
    // for any valid key: scalars in a list); fall back to a debug
    // rendering only if encoding an unexpected shape ever fails.
    acetone_model::keys::encode_key(std::slice::from_ref(value))
        .unwrap_or_else(|_| format!("{value:?}").into_bytes())
}

impl GraphSource for GraphSnapshot {
    fn all_nodes(&self) -> Vec<NodeValue> {
        self.nodes.clone()
    }

    fn nodes_by_labels(&self, labels: &[String]) -> Vec<NodeValue> {
        let Some((first, rest)) = labels.split_first() else {
            return self.nodes.clone();
        };
        // LabelScan on the (typically most selective) first label via the
        // index, then filter by any remaining labels.
        match self.by_label.get(first) {
            None => Vec::new(),
            Some(indices) => indices
                .iter()
                .map(|&i| &self.nodes[i])
                .filter(|node| rest.iter().all(|l| node.labels.contains(l)))
                .cloned()
                .collect(),
        }
    }

    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        // Walk only the edges incident to `node` (O(degree)), via the
        // adjacency indexes, not the whole edge set.
        let mut out = Vec::new();
        if matches!(direction, Direction::Out | Direction::Undirected)
            && let Some(indices) = self.out_edges.get(node)
        {
            out.extend(
                indices
                    .iter()
                    .filter_map(|&i| self.incident(i, node, types)),
            );
        }
        if matches!(direction, Direction::In | Direction::Undirected)
            && let Some(indices) = self.in_edges.get(node)
        {
            // A self-loop appears in both out_edges and in_edges; skip the
            // second sighting under Undirected so it is not double-counted.
            for &i in indices {
                if direction == Direction::Undirected && self.rels[i].start == *node {
                    continue;
                }
                out.extend(self.incident(i, node, types));
            }
        }
        out
    }

    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        self.by_id.get(id).map(|&i| self.nodes[i].clone())
    }

    fn nodes_by_index(&self, index_name: &str, value: &Value) -> Option<Vec<NodeValue>> {
        // Unknown index → the caller falls back to a label scan.
        let map = self.by_index.get(index_name)?;
        // A list value's equality recurses element-wise with the same Int/Float
        // cross-type rule (`[1] = [1.0]`), which an exact-byte bucket cannot
        // serve without enumerating 2^k element-type combinations. Fall back to
        // a scan for a list pin (`None`) — correct, just not accelerated.
        if matches!(value, Value::List(_)) {
            return None;
        }
        // A float pin that is an integer at or beyond 2^53 has a non-unique
        // integer preimage — many i64s round to the same f64 — so probing the
        // single `f as i64` would miss the others and under-select. Below 2^53
        // every integer is exactly representable, so the preimage is unique and
        // the Int/Float probe below is exact; at/above it, fall back to a scan.
        if let Value::Float(f) = value
            && f.fract() == 0.0
            && f.abs() >= 9_007_199_254_740_992.0
        {
            return None;
        }
        // The candidate byte keys whose stored values could equal `value` under
        // openCypher equality: for a number that means BOTH the Int and Float
        // encodings (3 = 3.0), since the index stores them under distinct keys.
        // The result is a candidate superset the caller still filters, so being
        // over-broad is safe; under-selecting would silently drop matches.
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for bytes in index_lookup_keys(value) {
            if let Some(indices) = map.get(&bytes) {
                for &i in indices {
                    if seen.insert(i) {
                        out.push(self.nodes[i].clone());
                    }
                }
            }
        }
        Some(out)
    }
}

/// The index byte keys whose stored value could equal a seek `value` under
/// openCypher equality. A number matches its own type *and* the other numeric
/// type (`3 = 3.0`); everything else matches only its own encoding. Null, NaN
/// and non-storable kinds yield no keys (select nothing — null/NaN-blind).
fn index_lookup_keys(value: &Value) -> Vec<Vec<u8>> {
    match value {
        Value::Int(n) => [
            encode_model_value(&ModelValue::Int(*n)),
            encode_model_value(&ModelValue::Float(*n as f64)),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Value::Float(f) => {
            let mut keys: Vec<Vec<u8>> = Vec::with_capacity(2);
            if let Some(k) = encode_model_value(&ModelValue::Float(*f)) {
                keys.push(k);
            }
            // An integer-valued float also equals the same integer.
            if f.is_finite()
                && f.fract() == 0.0
                && *f >= i64::MIN as f64
                && *f <= i64::MAX as f64
                && let Some(k) = encode_model_value(&ModelValue::Int(*f as i64))
            {
                keys.push(k);
            }
            keys
        }
        other => encode_index_value(other).into_iter().collect(),
    }
}

/// The memcomparable encoding of a runtime node's indexed property value, or
/// `None` when the node does not contribute an entry (does not bear the label,
/// property absent, or a null/NaN/non-scalar value). Uses the *runtime* value
/// (key properties already re-exposed, `Bytes`/temporal already rendered), so
/// it matches exactly what `node_satisfies` compares and what a seek probes.
fn index_value_bytes(node: &NodeValue, label: &str, property: &str) -> Option<Vec<u8>> {
    if !node.labels.iter().any(|l| l == label) {
        return None;
    }
    encode_index_value(node.properties.get(property)?)
}

/// Encode a runtime [`Value`] as an index key value, or `None` when it is not
/// index-eligible (null, NaN, or a non-storable kind — map/node/rel/path).
fn encode_index_value(value: &Value) -> Option<Vec<u8>> {
    encode_model_value(&model_value_of(value)?)
}

/// Encode a stored [`ModelValue`] as an index key value, or `None` when it is
/// null- or NaN-blind (both are excluded from the index).
fn encode_model_value(value: &ModelValue) -> Option<Vec<u8>> {
    if matches!(value, ModelValue::Null) {
        return None;
    }
    // A NaN anywhere makes the value unencodable (ADR-0004) → not indexed.
    acetone_model::keys::encode_key(std::slice::from_ref(value)).ok()
}

/// Convert a runtime [`Value`] to a stored [`ModelValue`], or `None` for a
/// kind that cannot be an index value (map/node/relationship/path) — or one
/// nested past [`MAX_VALUE_DEPTH`] (acetone-5xp): a runtime value can nest
/// arbitrarily deep (e.g. a reduce that wraps a list each step), and `None`
/// here safely means "not index-eligible", so the seek falls back to a scan
/// instead of this walk recursing without bound.
pub(crate) fn model_value_of(value: &Value) -> Option<ModelValue> {
    model_value_of_at(value, 0)
}

fn model_value_of_at(value: &Value, depth: usize) -> Option<ModelValue> {
    if depth >= MAX_VALUE_DEPTH {
        return None;
    }
    Some(match value {
        Value::Null => ModelValue::Null,
        Value::Bool(b) => ModelValue::Bool(*b),
        Value::Int(n) => ModelValue::Int(*n),
        Value::Float(x) => ModelValue::Float(*x),
        Value::String(s) => ModelValue::String(s.clone()),
        // Index keys mirror the *runtime* comparison, not storage: a carrier is
        // compared as its string rendering (it decays to a string before any
        // `=`), so it must be index-keyed as that same string — keying the raw
        // typed value would let a string-pinned seek miss it, disagreeing with a
        // scan (the invariant this whole converter exists to hold). Lossless
        // write-back is a separate concern, handled by `persist::convert_value`.
        Value::Stored(mv) => ModelValue::String(crate::exec::value::render_stored(mv)),
        Value::List(items) => ModelValue::List(
            items
                .iter()
                .map(|item| model_value_of_at(item, depth + 1))
                .collect::<Option<Vec<_>>>()?,
        ),
        Value::Map(_) | Value::Node(_) | Value::Relationship(_) | Value::Path(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Value as ExecValue, execute};
    use acetone_model::records::{EdgeRecord, NodeRecord};

    fn node_key(label: &str, key: &str) -> NodeKey {
        NodeKey::new(label, vec![ModelValue::String(key.into())]).unwrap()
    }

    fn snapshot() -> GraphSnapshot {
        let mut host_props = BTreeMap::new();
        host_props.insert("os".to_string(), ModelValue::String("debian".into()));
        let nodes = vec![
            (
                node_key("Host", "web-01"),
                NodeRecord::new(["Critical".to_string()], host_props),
            ),
            (
                node_key("Software", "nginx"),
                NodeRecord::new([], BTreeMap::new()),
            ),
        ];
        let edge = EdgeKey::new(
            node_key("Host", "web-01"),
            "RUNS",
            node_key("Software", "nginx"),
            ModelValue::Null,
        )
        .unwrap();
        let edges = vec![(edge, EdgeRecord::new(BTreeMap::new()))];
        GraphSnapshot::from_records(&nodes, &edges)
    }

    /// The runtime id of the `from -R-> to` edge in a snapshot, via `expand`.
    fn rel_id_of(snapshot: &GraphSnapshot, from: &NodeKey, to: &NodeKey) -> EntityId {
        use crate::ast::Direction;
        use crate::exec::source::GraphSource;
        let to_id = node_entity_id(to);
        snapshot
            .expand(&node_entity_id(from), Direction::Out, &["R".to_string()])
            .into_iter()
            .find(|(_, neighbour)| neighbour.id == to_id)
            .expect("the edge must be reachable")
            .0
            .id
    }

    #[test]
    fn relationship_identity_is_stable_across_snapshots() {
        // acetone-rid: a relationship's identity must derive from its edge key,
        // not its positional index — so inserting an unrelated *earlier* edge
        // must not renumber it. (With the old `e{index}` scheme it did.)
        let a = node_key("Host", "a");
        let b = node_key("Host", "b");
        let c = node_key("Host", "c");
        let nodes = vec![
            (a.clone(), NodeRecord::new([], BTreeMap::new())),
            (b.clone(), NodeRecord::new([], BTreeMap::new())),
            (c.clone(), NodeRecord::new([], BTreeMap::new())),
        ];
        let target = EdgeKey::new(b.clone(), "R", c.clone(), ModelValue::Null).unwrap();
        let earlier = EdgeKey::new(a.clone(), "R", b.clone(), ModelValue::Null).unwrap();

        // Snapshot 1: just the target edge.
        let s1 = GraphSnapshot::from_records(
            &nodes,
            &[(target.clone(), EdgeRecord::new(BTreeMap::new()))],
        );
        // Snapshot 2: an unrelated edge inserted *before* the target.
        let s2 = GraphSnapshot::from_records(
            &nodes,
            &[
                (earlier, EdgeRecord::new(BTreeMap::new())),
                (target, EdgeRecord::new(BTreeMap::new())),
            ],
        );

        assert_eq!(
            rel_id_of(&s1, &b, &c),
            rel_id_of(&s2, &b, &c),
            "relationship identity must not depend on unrelated earlier edges"
        );
    }

    #[test]
    fn a_relationship_id_never_equals_a_node_id() {
        // The edge id (encode_fwd bytes) and node id (node-key bytes) are
        // disjoint by construction — an edge encoding is strictly longer than
        // its source node's id and lives in a different structural shape. Lock
        // that down so rel/node identity can never be confused.
        use crate::exec::source::GraphSource;
        let a = node_key("Host", "a");
        let b = node_key("Host", "b");
        let nodes = vec![
            (a.clone(), NodeRecord::new([], BTreeMap::new())),
            (b.clone(), NodeRecord::new([], BTreeMap::new())),
        ];
        let ab = EdgeKey::new(a.clone(), "R", b.clone(), ModelValue::Null).unwrap();
        let s = GraphSnapshot::from_records(&nodes, &[(ab, EdgeRecord::new(BTreeMap::new()))]);
        let rel_id = rel_id_of(&s, &a, &b);
        let node_ids: Vec<EntityId> = s.all_nodes().into_iter().map(|n| n.id).collect();
        assert!(
            !node_ids.contains(&rel_id),
            "a relationship id must not collide with any node id"
        );
    }

    #[test]
    fn distinct_relationships_have_distinct_identities() {
        // The injective edge-key encoding must give parallel-endpoint and
        // different-endpoint edges distinct ids.
        let a = node_key("Host", "a");
        let b = node_key("Host", "b");
        let c = node_key("Host", "c");
        let nodes = vec![
            (a.clone(), NodeRecord::new([], BTreeMap::new())),
            (b.clone(), NodeRecord::new([], BTreeMap::new())),
            (c.clone(), NodeRecord::new([], BTreeMap::new())),
        ];
        let ab = EdgeKey::new(a.clone(), "R", b.clone(), ModelValue::Null).unwrap();
        let bc = EdgeKey::new(b.clone(), "R", c.clone(), ModelValue::Null).unwrap();
        let s = GraphSnapshot::from_records(
            &nodes,
            &[
                (ab, EdgeRecord::new(BTreeMap::new())),
                (bc, EdgeRecord::new(BTreeMap::new())),
            ],
        );
        assert_ne!(
            rel_id_of(&s, &a, &b),
            rel_id_of(&s, &b, &c),
            "distinct relationships must have distinct identities"
        );
    }

    #[test]
    fn key_properties_are_re_exposed_with_schema() {
        use acetone_model::schema::{LabelDef, SchemaEntry};

        let nodes = vec![(
            node_key("Host", "web-01"),
            NodeRecord::new([], BTreeMap::new()),
        )];
        let schema = vec![SchemaEntry::Label {
            name: "Host".into(),
            def: LabelDef::new(vec!["hostname".into()], BTreeMap::new(), [], []).unwrap(),
        }];

        // Without schema: the key value is not a queryable property.
        let plain = GraphSnapshot::from_records(&nodes, &[]);
        assert!(!plain.all_nodes()[0].properties.contains_key("hostname"));

        // With schema: the key value is re-exposed under its declared
        // property name, so `{hostname: 'web-01'}` and `RETURN h.hostname`
        // work.
        let with_schema = GraphSnapshot::from_records_with_schema(&nodes, &[], &schema);
        assert!(
            matches!(with_schema.all_nodes()[0].properties.get("hostname"),
                Some(Value::String(s)) if s == "web-01")
        );
    }

    #[test]
    fn virtual_diff_node_renders_a_removed_node_from_the_from_schema() {
        // acetone-v8g: the _Removed path renders the *before* record with the
        // `from` side's key names — the change label is prepended and the key
        // value is re-exposed under its declared property name.
        use acetone_model::schema::{LabelDef, SchemaEntry};

        let key = node_key("Host", "web-01");
        let record = NodeRecord::new(
            ["Critical".to_string()],
            BTreeMap::from([("os".to_string(), ModelValue::String("debian".into()))]),
        );
        let from_schema = vec![SchemaEntry::Label {
            name: "Host".into(),
            def: LabelDef::new(vec!["hostname".into()], BTreeMap::new(), [], []).unwrap(),
        }];
        let key_names = key_names_from_schema(&from_schema);

        let node = virtual_diff_node(&key, &record, &key_names, "_Removed");
        assert_eq!(
            node.labels,
            vec![
                "_Removed".to_string(),
                "Host".to_string(),
                "Critical".to_string()
            ],
            "change label first, then primary and secondary labels"
        );
        assert!(
            matches!(node.properties.get("hostname"), Some(Value::String(s)) if s == "web-01"),
            "the key value is re-exposed from the from-side schema"
        );
        assert!(
            matches!(node.properties.get("os"), Some(Value::String(s)) if s == "debian"),
            "record properties are preserved"
        );
    }

    #[test]
    fn virtual_diff_node_without_schema_keeps_the_node_but_not_key_properties() {
        // acetone-v8g: a schemaless version has no key names, so key
        // properties are not re-exposed — but the node is still rendered,
        // with its labels and record properties intact.
        let key = node_key("Thing", "seven");
        let record = NodeRecord::new([], BTreeMap::from([("v".to_string(), ModelValue::Int(42))]));
        let empty = key_names_from_schema(&[]);

        let node = virtual_diff_node(&key, &record, &empty, "_Added");
        assert_eq!(node.labels, vec!["_Added".to_string(), "Thing".to_string()]);
        assert!(
            matches!(node.properties.get("v"), Some(Value::Int(42))),
            "record properties survive without a schema"
        );
        // No schema names the key property, so nothing is re-exposed: the
        // only property is the record's.
        assert_eq!(
            node.properties.len(),
            1,
            "no key property can be re-exposed without a schema: {:?}",
            node.properties
        );
    }

    #[test]
    fn key_names_from_schema_collects_only_label_entries() {
        use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
        let schema = vec![
            SchemaEntry::Label {
                name: "Host".into(),
                def: LabelDef::new(vec!["hostname".into()], BTreeMap::new(), [], []).unwrap(),
            },
            SchemaEntry::Index {
                name: "by_os".into(),
                def: IndexDef::new("Host", vec!["os".into()]).unwrap(),
            },
        ];
        let key_names = key_names_from_schema(&schema);
        assert_eq!(key_names.len(), 1);
        assert_eq!(
            key_names.get("Host"),
            Some(&vec!["hostname".to_string()]),
            "the label's declared key tuple is mapped by label name"
        );
    }

    #[test]
    fn converts_records_to_queryable_nodes() {
        let snapshot = snapshot();
        assert_eq!(snapshot.node_count(), 2);
        assert_eq!(snapshot.rel_count(), 1);
        let host = snapshot
            .all_nodes()
            .into_iter()
            .find(|n| n.labels.contains(&"Host".to_string()))
            .unwrap();
        assert!(host.labels.contains(&"Critical".to_string()));
        assert!(matches!(host.properties.get("os"), Some(Value::String(s)) if s == "debian"));
    }

    #[test]
    fn executes_a_query_over_stored_records() {
        let snapshot = snapshot();
        let query = "MATCH (h:Host)-[:RUNS]->(s:Software) RETURN h.os, s";
        let parsed = crate::parse(query).unwrap();
        let bound = crate::bind::bind(
            query,
            &parsed,
            &Catalogue::empty(),
            crate::bind::BindMode::Lenient,
        )
        .unwrap();
        let result = execute(&bound, &snapshot, &BTreeMap::new()).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], ExecValue::String(s) if s == "debian"));
        assert!(matches!(&result.rows[0][1], ExecValue::Node(_)));
    }

    #[test]
    fn direction_and_labels_filter_expansion() {
        let snapshot = snapshot();
        // No incoming RUNS to a Host.
        let query = "MATCH (h:Host)<-[:RUNS]-(x) RETURN count(*) AS n";
        let parsed = crate::parse(query).unwrap();
        let bound = crate::bind::bind(
            query,
            &parsed,
            &Catalogue::empty(),
            crate::bind::BindMode::Lenient,
        )
        .unwrap();
        let result = execute(&bound, &snapshot, &BTreeMap::new()).unwrap();
        assert!(matches!(result.rows[0][0], ExecValue::Int(0)));
    }

    /// The adjacency index must match the old linear scan's semantics:
    /// direction filtering, self-loops counted once under an undirected
    /// match, and parallel edges each surfaced.
    #[test]
    fn indexed_expand_handles_self_loops_and_parallel_edges() {
        use crate::exec::source::GraphSource;

        let nodes = vec![
            (node_key("N", "a"), NodeRecord::new([], BTreeMap::new())),
            (node_key("N", "b"), NodeRecord::new([], BTreeMap::new())),
        ];
        // A self-loop on a, and two parallel a->b edges of different types.
        let edges = vec![
            (
                EdgeKey::new(
                    node_key("N", "a"),
                    "LOOP",
                    node_key("N", "a"),
                    ModelValue::Null,
                )
                .unwrap(),
                EdgeRecord::new(BTreeMap::new()),
            ),
            (
                EdgeKey::new(
                    node_key("N", "a"),
                    "R",
                    node_key("N", "b"),
                    ModelValue::Null,
                )
                .unwrap(),
                EdgeRecord::new(BTreeMap::new()),
            ),
            (
                EdgeKey::new(
                    node_key("N", "a"),
                    "S",
                    node_key("N", "b"),
                    ModelValue::Null,
                )
                .unwrap(),
                EdgeRecord::new(BTreeMap::new()),
            ),
        ];
        let snapshot = GraphSnapshot::from_records(&nodes, &edges);
        let a = node_entity_id(&node_key("N", "a"));

        // Outgoing from a: the loop + both parallel edges = 3.
        assert_eq!(snapshot.expand(&a, Direction::Out, &[]).len(), 3);
        // Undirected from a: the self-loop counts once (not twice), plus
        // the two parallel edges = 3.
        assert_eq!(snapshot.expand(&a, Direction::Undirected, &[]).len(), 3);
        // Incoming to a: only the self-loop.
        assert_eq!(snapshot.expand(&a, Direction::In, &[]).len(), 1);
        // Type filter selects one parallel edge.
        assert_eq!(
            snapshot
                .expand(&a, Direction::Out, &["R".to_string()])
                .len(),
            1
        );

        // The label index resolves the same nodes as a full scan.
        assert_eq!(snapshot.nodes_by_labels(&["N".to_string()]).len(), 2);
        assert_eq!(snapshot.nodes_by_labels(&["Missing".to_string()]).len(), 0);
        assert!(snapshot.node(&a).is_some());
    }

    /// Tear a nested list value down iteratively, so dropping the test's
    /// deliberately over-deep fixtures cannot itself recurse the drop glue
    /// off the stack.
    fn dismantle_model_value(value: ModelValue) {
        let mut stack = vec![value];
        while let Some(value) = stack.pop() {
            if let ModelValue::List(items) = value {
                stack.extend(items);
            }
        }
    }

    #[test]
    fn a_hostile_over_deep_stored_list_converts_bounded_not_overflowing() {
        // acetone-5xp: no decodable stored value can nest past the model's
        // encode/decode cap (128), so build one directly in memory to model a
        // hostile value that bypassed those caps. Conversion must terminate
        // with bounded recursion — content past MAX_VALUE_DEPTH degrades to
        // null — rather than smash the stack.
        let mut value = ModelValue::Int(7);
        for _ in 0..50_000 {
            value = ModelValue::List(vec![value]);
        }
        let converted = convert_value(&value);
        dismantle_model_value(value);

        // Walk down iteratively: exactly MAX_VALUE_DEPTH list levels
        // (depths 0..MAX_VALUE_DEPTH-1), then the guard's null.
        let mut levels = 0usize;
        let mut at = &converted;
        while let ExecValue::List(items) = at {
            assert_eq!(items.len(), 1);
            at = &items[0];
            levels += 1;
        }
        assert!(matches!(at, ExecValue::Null));
        assert_eq!(levels, MAX_VALUE_DEPTH);
    }

    #[test]
    fn a_model_cap_deep_stored_list_converts_losslessly() {
        // The defence-in-depth cap must never degrade a legitimate value: the
        // deepest list the model itself can encode/decode round-trips through
        // conversion intact, leaf included.
        let mut value = ModelValue::Int(7);
        for _ in 0..acetone_model::values::MAX_DEPTH {
            value = ModelValue::List(vec![value]);
        }
        let converted = convert_value(&value);
        let mut levels = 0usize;
        let mut at = &converted;
        while let ExecValue::List(items) = at {
            assert_eq!(items.len(), 1);
            at = &items[0];
            levels += 1;
        }
        assert!(matches!(at, ExecValue::Int(7)));
        assert_eq!(levels, acetone_model::values::MAX_DEPTH);
    }

    #[test]
    fn an_over_deep_runtime_value_is_not_index_eligible() {
        // model_value_of carries the same guard: an over-deep runtime seek
        // value converts to None (not index-eligible) instead of recursing —
        // semantically right, since no stored value can be that deep either.
        let mut value = ExecValue::Int(1);
        for _ in 0..50_000 {
            value = ExecValue::List(vec![value]);
        }
        assert!(model_value_of(&value).is_none());
        // Iterative teardown, as above.
        let mut stack = vec![value];
        while let Some(value) = stack.pop() {
            if let ExecValue::List(items) = value {
                stack.extend(items);
            }
        }
    }
}
