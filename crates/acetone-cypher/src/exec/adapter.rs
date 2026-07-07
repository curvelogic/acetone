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
        Self::build(nodes, edges, &HashMap::new())
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
        for entry in schema {
            if let SchemaEntry::Label { name, def } = entry {
                key_names.insert(name.clone(), def.key().to_vec());
            }
        }
        Self::build(nodes, edges, &key_names)
    }

    fn build(
        nodes: &[(NodeKey, NodeRecord)],
        edges: &[(EdgeKey, EdgeRecord)],
        key_names: &HashMap<String, Vec<String>>,
    ) -> Self {
        let node_values: Vec<NodeValue> = nodes
            .iter()
            .map(|(key, record)| node_value(key, record, key_names))
            .collect();
        let rel_values: Vec<RelValue> = edges
            .iter()
            .enumerate()
            .map(|(index, (key, record))| rel_value(index, key, record))
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

        GraphSnapshot {
            nodes: node_values,
            rels: rel_values,
            by_id,
            by_label,
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

/// Build a runtime node value for the diff virtual graph (acetone-14c.1):
/// the stored `(key, record)` rendered as a node — key properties re-exposed
/// under their schema-declared names — with a virtual change label
/// (`_Added`/`_Removed`/`_Modified`) prepended to its label set, so a query
/// can select it with `node:_Added`. `schema` is the version the record
/// belongs to (its `to` version for added/modified, `from` for removed).
pub fn virtual_diff_node(
    key: &NodeKey,
    record: &NodeRecord,
    schema: &[SchemaEntry],
    change_label: &str,
) -> NodeValue {
    let mut key_names: HashMap<String, Vec<String>> = HashMap::new();
    for entry in schema {
        if let SchemaEntry::Label { name, def } = entry {
            key_names.insert(name.clone(), def.key().to_vec());
        }
    }
    let mut node = node_value(key, record, &key_names);
    let mut labels = Vec::with_capacity(node.labels.len() + 1);
    labels.push(change_label.to_string());
    labels.append(&mut node.labels);
    node.labels = labels;
    node
}

fn node_value(
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

fn rel_value(index: usize, key: &EdgeKey, record: &EdgeRecord) -> RelValue {
    RelValue {
        // Edge identity: forward-key bytes plus the row index guard
        // against parallel edges sharing a discriminator collision in the
        // rendering (keys are unique, index only disambiguates the
        // rendering fallback).
        id: EntityId::from_bytes(format!("e{index}").into_bytes()),
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
/// (spec §5.1) defers temporal and byte types; rather than make a whole
/// node unqueryable, those render to strings (lossy, but property access
/// still works and temporal *arithmetic* is out of scope anyway).
fn convert_value(value: &ModelValue) -> Value {
    match value {
        ModelValue::Null => Value::Null,
        ModelValue::Bool(b) => Value::Bool(*b),
        ModelValue::Int(n) => Value::Int(*n),
        ModelValue::Float(x) => Value::Float(*x),
        ModelValue::String(s) => Value::String(s.clone()),
        ModelValue::List(items) => Value::List(items.iter().map(convert_value).collect()),
        ModelValue::Bytes(bytes) => Value::String(hex(bytes)),
        // Deferred temporal types: a stable string rendering.
        other => Value::String(format!("{other:?}")),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
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
}
