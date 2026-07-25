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
    /// (IndexSeek/IndexRange). Built for the schema's declared indexes,
    /// keyed by the memcomparable value encoding so lookups match the
    /// stored `idx/<name>` map's selection exactly (null/NaN-blind), and
    /// ordered so byte-range scans are value-ordered (Invariant #2:
    /// byte order == logical order within a type family).
    by_index: HashMap<String, std::collections::BTreeMap<Vec<u8>, Vec<usize>>>,
    /// `(label, runtime-faithful encoded key tuple)` → node indices
    /// (KeySeek). Keyed by the concatenated per-component encoding of the
    /// node's *runtime* key values — the same representation
    /// `node_satisfies` filters — so a seek and the filter agree on
    /// equality. Values are vectors: numeric cross-typing means distinct
    /// stored keys (`Int(1)`, `Float(1.0)`) can collide under runtime
    /// equality probes. Populated only when the schema names the label's
    /// key.
    by_key: HashMap<(String, Vec<u8>), Vec<usize>>,
    /// Index name → indexed property list (declared order), for
    /// validating hints bound against a different version's catalogue
    /// (PR #206 review finding 4).
    index_properties: HashMap<String, Vec<String>>,
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
        // Every declared index drives the in-memory seek — composite
        // indexes included (ADR-0027, acetone-0c7): the map keys are the
        // concatenated per-component encodings, which the self-delimiting
        // per-value encoding makes equal to the tuple encoding.
        let mut index_defs: Vec<(String, String, Vec<String>)> = Vec::new();
        for entry in schema {
            match entry {
                SchemaEntry::Label { name, def } => {
                    key_names.insert(name.clone(), def.key().to_vec());
                }
                SchemaEntry::Index { name, def } => {
                    index_defs.push((
                        name.clone(),
                        def.label().to_owned(),
                        def.properties().to_vec(),
                    ));
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
        index_defs: &[(String, String, Vec<String>)],
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
        let mut by_index: HashMap<String, std::collections::BTreeMap<Vec<u8>, Vec<usize>>> =
            HashMap::new();
        for (name, label, properties) in index_defs {
            let map = by_index.entry(name.clone()).or_default();
            for (index, node) in node_values.iter().enumerate() {
                let encoded: Option<Vec<u8>> = properties
                    .iter()
                    .map(|property| index_value_bytes(node, label, property))
                    .collect::<Option<Vec<Vec<u8>>>>()
                    .map(|parts| parts.concat());
                if let Some(bytes) = encoded {
                    map.entry(bytes).or_default().push(index);
                }
            }
        }

        // Primary-key seeks (KeySeek): only labels whose key the schema
        // names. Keys encode from the node's RUNTIME key values (the
        // re-exposed properties), per-component and concatenated — the
        // per-value encoding is self-delimiting, so concatenation equals
        // the tuple encoding — matching what probes can compute from a
        // pattern's pinned values.
        let mut by_key: HashMap<(String, Vec<u8>), Vec<usize>> = HashMap::new();
        for (index, (key, _)) in nodes.iter().enumerate() {
            let Some(names) = key_names.get(key.label()) else {
                continue;
            };
            let node = &node_values[index];
            let encoded: Option<Vec<u8>> = names
                .iter()
                .map(|name| node.properties.get(name).and_then(encode_index_value))
                .collect::<Option<Vec<Vec<u8>>>>()
                .map(|parts| parts.concat());
            if let Some(encoded) = encoded {
                by_key
                    .entry((key.label().to_owned(), encoded))
                    .or_default()
                    .push(index);
            }
        }

        let index_properties: HashMap<String, Vec<String>> = index_defs
            .iter()
            .map(|(name, _, properties)| (name.clone(), properties.clone()))
            .collect();

        GraphSnapshot {
            nodes: node_values,
            rels: rel_values,
            by_id,
            by_label,
            by_index,
            by_key,
            index_properties,
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

// The shared cap on runtime value nesting lives with the value type
// (acetone-19x); here it bounds the stored-value walks as defence in depth
// (acetone-5xp): stored values are bounded at
// `acetone_model::values::MAX_DEPTH` (128) by both the CBOR encoder and
// decoder, so no decodable value can reach this cap — the guard exists so
// that a hostile or corrupt value that somehow bypassed those caps meets a
// defined, non-panicking bound here instead of unbounded recursion (a stack
// smash) on the read path.
use crate::exec::value::MAX_VALUE_DEPTH;

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

    fn nodes_by_key(&self, label: &str, key_values: &[Value]) -> Option<Vec<NodeValue>> {
        // Candidate-superset semantics (PR #206 review finding 1): the
        // shared cartesian probe generalises the equality index's dual
        // numeric probe; a complete miss means "cannot serve" (scan),
        // never a definitive absence.
        let refs: Vec<&Value> = key_values.iter().collect();
        let probes = cartesian_probes(&refs)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut hit = false;
        for probe in probes {
            if let Some(indices) = self.by_key.get(&(label.to_owned(), probe)) {
                hit = true;
                for &i in indices {
                    if seen.insert(i) {
                        out.push(self.nodes[i].clone());
                    }
                }
            }
        }
        if hit { Some(out) } else { None }
    }

    fn nodes_by_index_range(
        &self,
        index_name: &str,
        property: &str,
        lower: Option<(&Value, bool)>,
        upper: Option<(&Value, bool)>,
    ) -> Option<Vec<NodeValue>> {
        // Ranges serve single-property indexes only: the registry entry
        // must be exactly [property].
        match self.index_properties.get(index_name) {
            Some(declared) if declared.len() == 1 && declared[0] == property => {}
            _ => return None,
        }
        let map = self.by_index.get(index_name)?;
        let ranges = range_families(lower, upper)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (start, end) in ranges {
            // An inverted range (`> 5 AND < 3`) selects nothing —
            // BTreeMap::range would panic on it.
            let start_bytes = match &start {
                std::ops::Bound::Included(b) | std::ops::Bound::Excluded(b) => b.as_slice(),
                std::ops::Bound::Unbounded => &[],
            };
            let end_bytes = match &end {
                std::ops::Bound::Included(b) | std::ops::Bound::Excluded(b) => b.as_slice(),
                std::ops::Bound::Unbounded => &[0xff],
            };
            if start_bytes > end_bytes {
                continue;
            }
            if start_bytes == end_bytes
                && matches!(
                    (&start, &end),
                    (std::ops::Bound::Excluded(_), _) | (_, std::ops::Bound::Excluded(_))
                )
            {
                continue;
            }
            for (_, indices) in map.range((start, end)) {
                for &i in indices {
                    if seen.insert(i) {
                        out.push(self.nodes[i].clone());
                    }
                }
            }
        }
        Some(out)
    }

    fn nodes_by_index(
        &self,
        index_name: &str,
        properties: &[String],
        values: &[&Value],
    ) -> Option<Vec<NodeValue>> {
        // Unknown index → the caller falls back to a label scan. A hint
        // bound against another version's catalogue must not be served by
        // a same-named index over different properties (finding 4), and
        // the value tuple must match the declared arity.
        if self.index_properties.get(index_name).map(Vec::as_slice) != Some(properties)
            || values.len() != properties.len()
        {
            return None;
        }
        let map = self.by_index.get(index_name)?;
        // Cartesian per-component probes, exactly as `nodes_by_key`: any
        // component with a possibly-incomplete probe set (list pin;
        // integral float >= 2^53) bails to a scan — a candidate superset
        // is safe, under-selection never is.
        let probes = cartesian_probes(values)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for probe in probes {
            if let Some(indices) = map.get(&probe) {
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

/// The bounded cartesian of per-component probe encodings for a pinned
/// value tuple (key seek and single/composite index seek alike): each
/// component contributes its runtime-equality-compatible encodings
/// (`index_lookup_keys` — both numeric families for a number), and the
/// concatenations enumerate every byte key a matching stored tuple could
/// have. Mirrored (over `ModelValue` tuples) by the store-backed
/// source's expansion in `store_source.rs` — keep their caps and bail
/// rules aligned. `None` when any component's probe set may be incomplete or the
/// product exceeds a small cap (the scan decides); `Some(empty)` when a
/// component has no encoding at all (null/NaN/unstorable) — such a pin
/// is null-blind and matches nothing, so an empty candidate set is the
/// correct, cheap answer.
fn cartesian_probes(values: &[&Value]) -> Option<Vec<Vec<u8>>> {
    if values.iter().any(|value| probe_set_incomplete(value)) {
        return None;
    }
    let per_component: Vec<Vec<Vec<u8>>> = values
        .iter()
        .map(|value| index_lookup_keys(value))
        .collect();
    if per_component
        .iter()
        .any(|alternatives| alternatives.is_empty())
    {
        return Some(Vec::new());
    }
    let combinations: usize = per_component.iter().map(Vec::len).product();
    if combinations > 16 {
        return None;
    }
    let mut probes: Vec<Vec<u8>> = vec![Vec::new()];
    for alternatives in &per_component {
        let mut next = Vec::with_capacity(probes.len() * alternatives.len());
        for prefix in &probes {
            for alt in alternatives {
                let mut bytes = prefix.clone();
                bytes.extend_from_slice(alt);
                next.push(bytes);
            }
        }
        probes = next;
    }
    Some(probes)
}

/// Whether `index_lookup_keys`' probe set for `value` may be INCOMPLETE
/// under openCypher equality — a float pin that is an integer at or
/// beyond 2^53 has a non-unique i64 preimage (many integers round to the
/// same f64 under `eq3`'s lossy comparison), so probing the single
/// `f as i64` would under-select. Every seek caller must bail to a scan
/// on this condition; sharing it here keeps the callers from diverging
/// (PR #206 review NEW-1). Also true for list pins, whose element-wise
/// cross-typing an exact-byte bucket cannot serve.
fn probe_set_incomplete(value: &Value) -> bool {
    match value {
        Value::List(_) => true,
        Value::Float(f) => f.fract() == 0.0 && f.abs() >= F64_EXACT_INT_LIMIT,
        _ => false,
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

/// The per-type-family byte ranges an index range scan must cover
/// (memcomparable encoding: byte order == logical order *within* a type
/// family; Int (0x04) and Float (0x05) are distinct families that both
/// hold numbers, so a numeric bound spans two ranges — mirroring the
/// equality seek's dual probe). `None` means the range cannot be served
/// (precision hazard) — fall back to a scan. Over-selection is safe (the
/// WHERE still filters); under-selection is a bug.
#[allow(clippy::type_complexity)]
fn range_families(
    lower: Option<(&Value, bool)>,
    upper: Option<(&Value, bool)>,
) -> Option<Vec<(std::ops::Bound<Vec<u8>>, std::ops::Bound<Vec<u8>>)>> {
    use std::ops::Bound;
    let numeric = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_));
    let is_num_bound = |b: &Option<(&Value, bool)>| b.map(|(v, _)| numeric(v)).unwrap_or(true);
    // A null/NaN endpoint compares as null/false against everything: the
    // predicate can never hold, so select nothing.
    let degenerate = |b: &Option<(&Value, bool)>| {
        b.map(|(v, _)| matches!(v, Value::Null) || matches!(v, Value::Float(f) if f.is_nan()))
            .unwrap_or(false)
    };
    if degenerate(&lower) || degenerate(&upper) {
        return Some(Vec::new());
    }
    let fully_open = lower.is_none() && upper.is_none();
    if fully_open {
        // No bound at all — nothing to prune; let the caller scan.
        return None;
    }
    if is_num_bound(&lower) && is_num_bound(&upper) && (lower.is_some() || upper.is_some()) {
        // Either family hitting a precision hazard bails to a scan.
        let int_range = int_family_range(lower, upper)?;
        let float_range = float_family_range(lower, upper)?;
        return Some([int_range, float_range].into_iter().flatten().collect());
    }
    // Non-numeric: both present bounds must share the encoded type family,
    // else the comparison is null everywhere — select nothing.
    let encode = |v: &Value| encode_index_value(v);
    let family_of = |bytes: &[u8]| bytes.first().copied();
    let lower_enc = match lower {
        None => None,
        Some((v, inc)) => match encode(v) {
            Some(b) => Some((b, inc)),
            None => return Some(Vec::new()),
        },
    };
    let upper_enc = match upper {
        None => None,
        Some((v, inc)) => match encode(v) {
            Some(b) => Some((b, inc)),
            None => return Some(Vec::new()),
        },
    };
    let family = match (&lower_enc, &upper_enc) {
        (Some((l, _)), Some((u, _))) => {
            if family_of(l) != family_of(u) {
                return Some(Vec::new());
            }
            family_of(l)?
        }
        (Some((l, _)), None) => family_of(l)?,
        (None, Some((u, _))) => family_of(u)?,
        (None, None) => return None,
    };
    let start = match lower_enc {
        Some((b, true)) => Bound::Included(b),
        Some((b, false)) => Bound::Excluded(b),
        None => Bound::Included(vec![family]),
    };
    let end = match upper_enc {
        Some((b, true)) => Bound::Included(b),
        Some((b, false)) => Bound::Excluded(b),
        None => Bound::Excluded(vec![family + 1]),
    };
    Some(vec![(start, end)])
}

/// Integers exactly representable as f64 end here; beyond it the
/// Int↔Float bound conversions lose precision, so range pruning bails.
const F64_EXACT_INT_LIMIT: f64 = 9_007_199_254_740_992.0;

/// The Int-family (tag 0x04) byte range for numeric bounds, or `None` on
/// a precision hazard. The outer Option wraps a possibly-empty range.
#[allow(clippy::type_complexity)]
fn int_family_range(
    lower: Option<(&Value, bool)>,
    upper: Option<(&Value, bool)>,
) -> Option<Option<(std::ops::Bound<Vec<u8>>, std::ops::Bound<Vec<u8>>)>> {
    use std::ops::Bound;
    let enc = |n: i64| encode_model_value(&ModelValue::Int(n));
    let start = match lower {
        None => Bound::Included(vec![0x04]),
        Some((Value::Int(n), true)) => Bound::Included(enc(*n)?),
        Some((Value::Int(n), false)) => Bound::Excluded(enc(*n)?),
        Some((Value::Float(f), _)) if f.abs() >= F64_EXACT_INT_LIMIT => return None,
        Some((Value::Float(f), inclusive)) => {
            // The smallest int i with i > f (or i >= f when inclusive).
            let i = if f.fract() == 0.0 {
                let n = *f as i64;
                if inclusive { n } else { n.checked_add(1)? }
            } else {
                f.ceil() as i64
            };
            Bound::Included(enc(i)?)
        }
        Some(_) => return None,
    };
    let end = match upper {
        None => Bound::Excluded(vec![0x05]),
        Some((Value::Int(n), true)) => Bound::Included(enc(*n)?),
        Some((Value::Int(n), false)) => Bound::Excluded(enc(*n)?),
        Some((Value::Float(f), _)) if f.abs() >= F64_EXACT_INT_LIMIT => return None,
        Some((Value::Float(f), inclusive)) => {
            // The largest int i with i < f (or i <= f when inclusive).
            let i = if f.fract() == 0.0 {
                let n = *f as i64;
                if inclusive { n } else { n.checked_sub(1)? }
            } else {
                f.floor() as i64
            };
            Bound::Included(enc(i)?)
        }
        Some(_) => return None,
    };
    Some(Some((start, end)))
}

/// The Float-family (tag 0x05) byte range for numeric bounds, or `None`
/// on a precision hazard. Zero bounds widen across the -0.0/+0.0
/// total-order split (they are numerically equal but encode apart), so a
/// `>= 0.0` range still selects stored `-0.0` values.
#[allow(clippy::type_complexity)]
fn float_family_range(
    lower: Option<(&Value, bool)>,
    upper: Option<(&Value, bool)>,
) -> Option<Option<(std::ops::Bound<Vec<u8>>, std::ops::Bound<Vec<u8>>)>> {
    use std::ops::Bound;
    let as_f64 = |v: &Value| -> Option<f64> {
        match v {
            Value::Float(f) => Some(*f),
            Value::Int(n) => {
                if (n.unsigned_abs() as f64) >= F64_EXACT_INT_LIMIT {
                    None
                } else {
                    Some(*n as f64)
                }
            }
            _ => None,
        }
    };
    let enc = |f: f64| encode_model_value(&ModelValue::Float(f));
    let start = match lower {
        None => Bound::Included(vec![0x05]),
        Some((v, inclusive)) => {
            let mut f = as_f64(v)?;
            if f == 0.0 {
                f = -0.0; // widen across the total-order zero split
            }
            if inclusive {
                Bound::Included(enc(f)?)
            } else if f == 0.0 {
                // `> 0.0` must still exclude +0.0 AND -0.0 (equal), but
                // the byte range from -0.0-exclusive would include +0.0.
                // Start above +0.0 instead.
                Bound::Excluded(enc(0.0)?)
            } else {
                Bound::Excluded(enc(f)?)
            }
        }
    };
    let end = match upper {
        None => Bound::Excluded(vec![0x06]),
        Some((v, inclusive)) => {
            let mut f = as_f64(v)?;
            if f == 0.0 {
                f = 0.0_f64.copysign(1.0); // +0.0: widen upward
            }
            if inclusive {
                Bound::Included(enc(f)?)
            } else if f == 0.0 {
                // `< 0.0` must exclude both zeros: end below -0.0.
                Bound::Excluded(enc(-0.0)?)
            } else {
                Bound::Excluded(enc(f)?)
            }
        }
    };
    Some(Some((start, end)))
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
    /// A snapshot with a declared schema: keyed Host nodes carrying an
    /// indexed numeric `cores` (mixed Int/Float) and string `os`.
    fn indexed_snapshot() -> GraphSnapshot {
        use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
        let mut nodes = Vec::new();
        let values: [(&str, ModelValue); 6] = [
            ("h1", ModelValue::Int(2)),
            ("h2", ModelValue::Int(4)),
            ("h3", ModelValue::Float(4.5)),
            ("h4", ModelValue::Int(8)),
            ("h5", ModelValue::Float(-0.0)),
            ("h6", ModelValue::Int(0)),
        ];
        for (name, cores) in values {
            let mut props = BTreeMap::new();
            props.insert("cores".to_string(), cores);
            props.insert("os".to_string(), ModelValue::String(format!("os-{name}")));
            nodes.push((node_key("Host", name), NodeRecord::new([], props)));
        }
        let schema = vec![
            SchemaEntry::Label {
                name: "Host".into(),
                def: LabelDef::new(vec!["hostname".into()], BTreeMap::new(), [], []).unwrap(),
            },
            SchemaEntry::Index {
                name: "by_cores".into(),
                def: IndexDef::new("Host", vec!["cores".into()]).unwrap(),
            },
        ];
        GraphSnapshot::from_records_with_schema(&nodes, &[], &schema)
    }

    fn range_names(
        snapshot: &GraphSnapshot,
        lower: Option<(&Value, bool)>,
        upper: Option<(&Value, bool)>,
    ) -> Vec<String> {
        use crate::exec::source::GraphSource;
        let mut names: Vec<String> = snapshot
            .nodes_by_index_range("by_cores", "cores", lower, upper)
            .expect("index exists")
            .into_iter()
            .filter_map(|n| match n.properties.get("hostname") {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn index_range_spans_both_numeric_families() {
        let snapshot = indexed_snapshot();
        // (2, 8): excludes both endpoints, spans Int 4 and Float 4.5.
        let lower = Value::Int(2);
        let upper = Value::Int(8);
        assert_eq!(
            range_names(&snapshot, Some((&lower, false)), Some((&upper, false))),
            vec!["h2", "h3"]
        );
        // >= 4.5 with a float bound: Int 8 must appear (cross-family).
        let lower = Value::Float(4.5);
        assert_eq!(
            range_names(&snapshot, Some((&lower, true)), None),
            vec!["h3", "h4"]
        );
        // <= 4 inclusive: h1, h2, h5(-0.0), h6(0).
        let upper = Value::Int(4);
        assert_eq!(
            range_names(&snapshot, None, Some((&upper, true))),
            vec!["h1", "h2", "h5", "h6"]
        );
    }

    #[test]
    fn index_range_handles_zero_and_degenerates() {
        let snapshot = indexed_snapshot();
        // >= 0 must select the stored -0.0 (numerically equal) and 0.
        let zero = Value::Int(0);
        assert_eq!(
            range_names(&snapshot, Some((&zero, true)), None),
            vec!["h1", "h2", "h3", "h4", "h5", "h6"]
        );
        // > 0 must exclude BOTH zeros.
        assert_eq!(
            range_names(&snapshot, Some((&zero, false)), None),
            vec!["h1", "h2", "h3", "h4"]
        );
        let zero_f = Value::Float(0.0);
        // < 0.0 excludes both zeros (nothing negative stored).
        assert!(range_names(&snapshot, None, Some((&zero_f, false))).is_empty());
        // Inverted range selects nothing (and must not panic).
        let five = Value::Int(5);
        let three = Value::Int(3);
        assert!(range_names(&snapshot, Some((&five, false)), Some((&three, false))).is_empty());
        // Null/NaN endpoints select nothing.
        let null = Value::Null;
        assert!(range_names(&snapshot, Some((&null, true)), None).is_empty());
        let nan = Value::Float(f64::NAN);
        assert!(range_names(&snapshot, None, Some((&nan, true))).is_empty());
        // Mixed-family bounds (number vs string) select nothing.
        let s = Value::String("a".into());
        let one = Value::Int(1);
        assert!(range_names(&snapshot, Some((&one, true)), Some((&s, true))).is_empty());
    }

    #[test]
    fn key_seek_candidates_and_fallbacks() {
        use crate::exec::source::GraphSource;
        let snapshot = indexed_snapshot();
        // Present key: exactly the node.
        let found = snapshot
            .nodes_by_key("Host", &[Value::String("h3".into())])
            .expect("seekable");
        assert_eq!(found.len(), 1);
        assert!(matches!(
            found[0].properties.get("cores"),
            Some(Value::Float(f)) if *f == 4.5
        ));
        // A miss is NEVER a definitive absence — the caller scans
        // (PR #206 review finding 1).
        assert!(
            snapshot
                .nodes_by_key("Host", &[Value::String("nope".into())])
                .is_none()
        );
        // A label the schema gave no key: cannot seek, fall back.
        assert!(
            snapshot
                .nodes_by_key("Software", &[Value::Int(1)])
                .is_none()
        );
        // A null key value has no encoding: scan decides.
        assert!(snapshot.nodes_by_key("Host", &[Value::Null]).is_none());
    }

    /// Cross-type key pins through full execution: hinted results must
    /// equal scanned results for `{id: 1.0}` vs a stored `Int(1)` key and
    /// vice versa — the exact row-drops of PR #206 review finding 1.
    #[test]
    fn cross_type_key_pins_match_scan_end_to_end() {
        use acetone_model::schema::{LabelDef, SchemaEntry};
        let nodes = vec![
            (
                NodeKey::new("Item", vec![ModelValue::Int(1)]).unwrap(),
                NodeRecord::new([], BTreeMap::new()),
            ),
            (
                NodeKey::new("Item", vec![ModelValue::Float(2.0)]).unwrap(),
                NodeRecord::new([], BTreeMap::new()),
            ),
            (
                NodeKey::new("Item", vec![ModelValue::Float(-0.0)]).unwrap(),
                NodeRecord::new([], BTreeMap::new()),
            ),
        ];
        let schema = vec![SchemaEntry::Label {
            name: "Item".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).unwrap(),
        }];
        let snapshot = GraphSnapshot::from_records_with_schema(&nodes, &[], &schema);
        let with_schema = Catalogue::from_entries(schema);
        for query in [
            "MATCH (i:Item {id: 1.0}) RETURN i.id",
            "MATCH (i:Item {id: 1}) RETURN i.id",
            "MATCH (i:Item {id: 2}) RETURN i.id",
            "MATCH (i:Item {id: 0}) RETURN i.id",
            "MATCH (i:Item {id: 'absent'}) RETURN i.id",
        ] {
            let parsed = crate::parse(query).unwrap();
            let hinted = {
                let bound =
                    crate::bind::bind(query, &parsed, &with_schema, crate::bind::BindMode::Strict)
                        .unwrap();
                execute(&bound, &snapshot, &BTreeMap::new()).unwrap()
            };
            let scanned = {
                let bound = crate::bind::bind(
                    query,
                    &parsed,
                    &Catalogue::empty(),
                    crate::bind::BindMode::Lenient,
                )
                .unwrap();
                execute(&bound, &snapshot, &BTreeMap::new()).unwrap()
            };
            assert_eq!(
                format!("{:?}", hinted.rows),
                format!("{:?}", scanned.rows),
                "{query}"
            );
        }
    }

    /// The precision edge (PR #206 review NEW-1): an integral float pin
    /// at/beyond 2^53 has a non-unique i64 preimage, so the key seek must
    /// bail to a scan — and end-to-end results must match the scan.
    #[test]
    fn key_seek_bails_at_the_float_precision_edge() {
        use crate::exec::source::GraphSource;
        use acetone_model::schema::{LabelDef, SchemaEntry};
        const EDGE: i64 = 9_007_199_254_740_992; // 2^53
        let nodes = vec![
            (
                NodeKey::new("Item", vec![ModelValue::Int(EDGE)]).unwrap(),
                NodeRecord::new([], BTreeMap::new()),
            ),
            (
                NodeKey::new("Item", vec![ModelValue::Int(EDGE + 1)]).unwrap(),
                NodeRecord::new([], BTreeMap::new()),
            ),
        ];
        let schema = vec![SchemaEntry::Label {
            name: "Item".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).unwrap(),
        }];
        let snapshot = GraphSnapshot::from_records_with_schema(&nodes, &[], &schema);
        // The float pin cannot be served: both stored Int keys are
        // lossy-equal to it. Bail (scan), never a partial candidate set.
        assert!(
            snapshot
                .nodes_by_key("Item", &[Value::Float(EDGE as f64)])
                .is_none()
        );
        // End to end, hinted equals scanned.
        let with_schema = Catalogue::from_entries(schema);
        let query = "MATCH (i:Item {id: 9007199254740992.0}) RETURN i.id ORDER BY i.id";
        let parsed = crate::parse(query).unwrap();
        let hinted = {
            let bound =
                crate::bind::bind(query, &parsed, &with_schema, crate::bind::BindMode::Strict)
                    .unwrap();
            execute(&bound, &snapshot, &BTreeMap::new()).unwrap()
        };
        let scanned = {
            let bound = crate::bind::bind(
                query,
                &parsed,
                &Catalogue::empty(),
                crate::bind::BindMode::Lenient,
            )
            .unwrap();
            execute(&bound, &snapshot, &BTreeMap::new()).unwrap()
        };
        assert_eq!(hinted.rows.len(), 2, "both lossy-equal keys match");
        assert_eq!(format!("{:?}", hinted.rows), format!("{:?}", scanned.rows));
    }

    /// Numeric cross-typing: a pin of either numeric type must reach keys
    /// stored under both families — `{id: 1.0}` matches a key `Int(1)`,
    /// and a pin `{id: 1}` returns BOTH a key `Int(1)` and a distinct
    /// node keyed `Float(1.0)` (PR #206 review finding 1).
    #[test]
    fn key_seek_probes_numeric_cross_types() {
        use crate::exec::source::GraphSource;
        use acetone_model::schema::{LabelDef, SchemaEntry};
        let nodes = vec![
            (
                NodeKey::new("Item", vec![ModelValue::Int(1)]).unwrap(),
                NodeRecord::new([], BTreeMap::new()),
            ),
            (
                NodeKey::new("Item", vec![ModelValue::Float(1.0)]).unwrap(),
                NodeRecord::new([], BTreeMap::new()),
            ),
            (
                NodeKey::new("Item", vec![ModelValue::Float(-0.0)]).unwrap(),
                NodeRecord::new([], BTreeMap::new()),
            ),
        ];
        let schema = vec![SchemaEntry::Label {
            name: "Item".into(),
            def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).unwrap(),
        }];
        let snapshot = GraphSnapshot::from_records_with_schema(&nodes, &[], &schema);
        // Either numeric pin reaches both same-value keys.
        let candidates = snapshot
            .nodes_by_key("Item", &[Value::Float(1.0)])
            .expect("hit");
        assert_eq!(candidates.len(), 2);
        let candidates = snapshot
            .nodes_by_key("Item", &[Value::Int(1)])
            .expect("hit");
        assert_eq!(candidates.len(), 2);
        // A zero pin reaches the -0.0 key (numerically equal).
        let candidates = snapshot
            .nodes_by_key("Item", &[Value::Int(0)])
            .expect("hit");
        assert_eq!(candidates.len(), 1);
    }

    /// End-to-end parity: range- and key-hinted execution returns exactly
    /// the rows a plain scan does — the hint only prunes anchors.
    #[test]
    fn hinted_execution_matches_scan_results() {
        use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
        let snapshot = indexed_snapshot();
        let schema = vec![
            SchemaEntry::Label {
                name: "Host".into(),
                def: LabelDef::new(vec!["hostname".into()], BTreeMap::new(), [], []).unwrap(),
            },
            SchemaEntry::Index {
                name: "by_cores".into(),
                def: IndexDef::new("Host", vec!["cores".into()]).unwrap(),
            },
        ];
        let with_index = Catalogue::from_entries(schema);
        let queries = [
            "MATCH (h:Host) WHERE h.cores > 2 AND h.cores <= 4.5 RETURN h.hostname ORDER BY h.hostname",
            "MATCH (h:Host) WHERE h.cores >= 0 RETURN h.hostname ORDER BY h.hostname",
            "MATCH (h:Host {hostname: 'h4'}) RETURN h.cores",
            "MATCH (h:Host {hostname: 'absent'}) RETURN h.cores",
        ];
        // With MutableGraph forwarding the seeks (PR #206 review finding
        // 2), this parity harness genuinely drives the hinted paths.
        for query in queries {
            let parsed = crate::parse(query).unwrap();
            let hinted = {
                let bound =
                    crate::bind::bind(query, &parsed, &with_index, crate::bind::BindMode::Strict)
                        .unwrap();
                execute(&bound, &snapshot, &BTreeMap::new()).unwrap()
            };
            let scanned = {
                let bound = crate::bind::bind(
                    query,
                    &parsed,
                    &Catalogue::empty(),
                    crate::bind::BindMode::Lenient,
                )
                .unwrap();
                execute(&bound, &snapshot, &BTreeMap::new()).unwrap()
            };
            assert_eq!(
                format!("{:?}", hinted.rows),
                format!("{:?}", scanned.rows),
                "{query}"
            );
        }
    }

    /// The range path serves single-property indexes only: a composite
    /// registry entry must refuse a range (an AT/version-skew hint could
    /// otherwise range-scan a composite-keyed map with single-value
    /// bounds and under-select arbitrarily — PR #207 review).
    #[test]
    fn index_range_refuses_composite_indexes() {
        use crate::exec::source::GraphSource;
        use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
        let mut props = BTreeMap::new();
        props.insert("region".to_string(), ModelValue::String("eu".into()));
        props.insert("port".to_string(), ModelValue::Int(80));
        let nodes = vec![(node_key("Host", "a"), NodeRecord::new([], props))];
        let schema = vec![
            SchemaEntry::Label {
                name: "Host".into(),
                def: LabelDef::new(vec!["hostname".into()], BTreeMap::new(), [], []).unwrap(),
            },
            SchemaEntry::Index {
                name: "by_region_port".into(),
                def: IndexDef::new("Host", vec!["region".into(), "port".into()]).unwrap(),
            },
        ];
        let snapshot = GraphSnapshot::from_records_with_schema(&nodes, &[], &schema);
        let lower = Value::Int(1);
        assert!(
            snapshot
                .nodes_by_index_range("by_region_port", "port", Some((&lower, true)), None)
                .is_none()
        );
        assert!(
            snapshot
                .nodes_by_index_range("by_region_port", "region", Some((&lower, true)), None)
                .is_none()
        );
    }

    /// Composite index seek (acetone-0c7): an all-properties-pinned
    /// composite index serves the seek, with per-component numeric
    /// cross-typing, and hinted results equal scanned results.
    #[test]
    fn composite_index_seek_matches_scan() {
        use crate::exec::source::GraphSource;
        use acetone_model::schema::{IndexDef, LabelDef, SchemaEntry};
        let mut nodes = Vec::new();
        for (name, region, port) in [
            ("a", "eu", ModelValue::Int(80)),
            ("b", "eu", ModelValue::Int(443)),
            ("c", "us", ModelValue::Int(80)),
            ("d", "eu", ModelValue::Float(80.0)),
        ] {
            let mut props = BTreeMap::new();
            props.insert("region".to_string(), ModelValue::String(region.into()));
            props.insert("port".to_string(), port);
            nodes.push((node_key("Host", name), NodeRecord::new([], props)));
        }
        let schema = vec![
            SchemaEntry::Label {
                name: "Host".into(),
                def: LabelDef::new(vec!["hostname".into()], BTreeMap::new(), [], []).unwrap(),
            },
            SchemaEntry::Index {
                name: "by_region_port".into(),
                def: IndexDef::new("Host", vec!["region".into(), "port".into()]).unwrap(),
            },
        ];
        let snapshot = GraphSnapshot::from_records_with_schema(&nodes, &[], &schema);
        // Direct: an Int pin reaches BOTH the Int(80) and Float(80.0)
        // entries under ("eu", 80) — per-component cross-typing.
        let region = Value::String("eu".into());
        let port = Value::Int(80);
        let got = snapshot
            .nodes_by_index(
                "by_region_port",
                &["region".into(), "port".into()],
                &[&region, &port],
            )
            .expect("served");
        assert_eq!(got.len(), 2);
        // Arity/property-list mismatches refuse (scan fallback).
        assert!(
            snapshot
                .nodes_by_index("by_region_port", &["region".into()], &[&region])
                .is_none()
        );
        // End to end: hinted equals scanned, including the cross-typed row.
        let with_schema = Catalogue::from_entries(schema);
        for query in [
            "MATCH (h:Host {region: 'eu', port: 80}) RETURN h.hostname ORDER BY h.hostname",
            "MATCH (h:Host {region: 'eu', port: 443}) RETURN h.hostname",
            "MATCH (h:Host {region: 'nowhere', port: 80}) RETURN h.hostname",
        ] {
            let parsed = crate::parse(query).unwrap();
            let hinted = {
                let bound =
                    crate::bind::bind(query, &parsed, &with_schema, crate::bind::BindMode::Strict)
                        .unwrap();
                execute(&bound, &snapshot, &BTreeMap::new()).unwrap()
            };
            let scanned = {
                let bound = crate::bind::bind(
                    query,
                    &parsed,
                    &Catalogue::empty(),
                    crate::bind::BindMode::Lenient,
                )
                .unwrap();
                execute(&bound, &snapshot, &BTreeMap::new()).unwrap()
            };
            assert_eq!(
                format!("{:?}", hinted.rows),
                format!("{:?}", scanned.rows),
                "{query}"
            );
        }
    }
}
