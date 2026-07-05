//! Bridges stored graph records (acetone-graph / acetone-model) to the
//! executor's [`GraphSource`]. Builds a materialised in-memory snapshot
//! once — at workbench scale (spec §1) that is cheap, and it decouples
//! execution from the storage layer's lifetimes. A streaming provider is
//! a later optimisation.
//!
//! `AT <ref>` whole-query time travel is served by the caller choosing
//! which stored version's records to hand in (the CLI reads at a resolved
//! ref); clause-group `AT` inside a query stays with acetone-yzc.7.

use std::collections::BTreeMap;

use acetone_model::Value as ModelValue;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::SchemaEntry;

use crate::ast::Direction;
use crate::bind::Catalogue;
use crate::exec::source::GraphSource;
use crate::exec::value::{EntityId, NodeValue, RelValue, Value};

/// A materialised snapshot of a stored graph version, ready to execute
/// against.
#[derive(Debug, Default)]
pub struct GraphSnapshot {
    nodes: Vec<NodeValue>,
    rels: Vec<RelValue>,
}

impl GraphSnapshot {
    /// Build from a version's node and edge records (e.g. from a
    /// `Repository`/`Snapshot`'s `nodes()` and `edges()`).
    pub fn from_records(nodes: &[(NodeKey, NodeRecord)], edges: &[(EdgeKey, EdgeRecord)]) -> Self {
        let node_values = nodes
            .iter()
            .map(|(key, record)| node_value(key, record))
            .collect();
        let rel_values = edges
            .iter()
            .enumerate()
            .map(|(index, (key, record))| rel_value(index, key, record))
            .collect();
        GraphSnapshot {
            nodes: node_values,
            rels: rel_values,
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn rel_count(&self) -> usize {
        self.rels.len()
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

fn node_value(key: &NodeKey, record: &NodeRecord) -> NodeValue {
    let mut labels = Vec::with_capacity(1 + record.secondary_labels().len());
    labels.push(key.label().to_string());
    labels.extend(record.secondary_labels().iter().cloned());
    // Key properties are part of identity but also queryable as named
    // properties would be in a real schema; the record carries the
    // non-key properties, which is what property access sees.
    NodeValue {
        id: node_entity_id(key),
        labels,
        properties: convert_map(record.properties()),
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

    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        let mut out = Vec::new();
        for rel in &self.rels {
            if !types.is_empty() && !types.contains(&rel.rel_type) {
                continue;
            }
            let neighbour = match direction {
                Direction::Out if rel.start == *node => &rel.end,
                Direction::In if rel.end == *node => &rel.start,
                Direction::Undirected if rel.start == *node => &rel.end,
                Direction::Undirected if rel.end == *node => &rel.start,
                _ => continue,
            };
            if let Some(neighbour) = self.node(neighbour) {
                out.push((rel.clone(), neighbour));
            }
        }
        out
    }

    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        self.nodes.iter().find(|n| n.id == *id).cloned()
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
}
