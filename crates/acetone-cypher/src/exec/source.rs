//! The executor's view of a graph: provider-pluggable per spec §9's
//! virtual-element note. Implementations: [`MemoryGraph`] (tests, TCK
//! backend) and, on the workbench path, an adapter over acetone-graph
//! read transactions (arrives with the AT/time-travel bead, which owns
//! the ref-resolution plumbing).

use std::collections::BTreeMap;

use crate::ast::Direction;
use crate::exec::value::{EntityId, NodeValue, RelValue, Value};

/// Read access to one graph state. Object-safe; the executor holds
/// `&dyn GraphSource`.
pub trait GraphSource {
    /// Every node, in a stable order.
    fn all_nodes(&self) -> Vec<NodeValue>;

    /// Nodes carrying every one of `labels` (empty set = all nodes).
    fn nodes_by_labels(&self, labels: &[String]) -> Vec<NodeValue> {
        self.all_nodes()
            .into_iter()
            .filter(|node| labels.iter().all(|l| node.labels.contains(l)))
            .collect()
    }

    /// Relationships incident to `node` in `direction`, filtered to
    /// `types` when non-empty. Each result is (relationship, neighbour).
    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)>;

    /// A node by id (path/pattern re-anchoring).
    fn node(&self, id: &EntityId) -> Option<NodeValue>;
}

/// A simple in-memory property graph.
#[derive(Debug, Default)]
pub struct MemoryGraph {
    nodes: Vec<NodeValue>,
    rels: Vec<RelValue>,
}

impl MemoryGraph {
    pub fn new() -> Self {
        MemoryGraph::default()
    }

    pub fn add_node(
        &mut self,
        labels: impl IntoIterator<Item = impl Into<String>>,
        properties: BTreeMap<String, Value>,
    ) -> EntityId {
        let id = EntityId::from_bytes(format!("n{}", self.nodes.len()).into_bytes());
        self.nodes.push(NodeValue {
            id: id.clone(),
            labels: labels.into_iter().map(Into::into).collect(),
            properties,
        });
        id
    }

    pub fn add_rel(
        &mut self,
        start: &EntityId,
        rel_type: impl Into<String>,
        end: &EntityId,
        properties: BTreeMap<String, Value>,
    ) -> EntityId {
        let id = EntityId::from_bytes(format!("r{}", self.rels.len()).into_bytes());
        self.rels.push(RelValue {
            id: id.clone(),
            rel_type: rel_type.into(),
            start: start.clone(),
            end: end.clone(),
            properties,
        });
        id
    }
}

impl GraphSource for MemoryGraph {
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

/// The empty graph — the TCK's most common fixture.
pub struct EmptyGraph;

impl GraphSource for EmptyGraph {
    fn all_nodes(&self) -> Vec<NodeValue> {
        Vec::new()
    }

    fn expand(&self, _: &EntityId, _: Direction, _: &[String]) -> Vec<(RelValue, NodeValue)> {
        Vec::new()
    }

    fn node(&self, _: &EntityId) -> Option<NodeValue> {
        None
    }
}
