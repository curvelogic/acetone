//! The write-execution model (spec §5.1 Level W, Phase 3).
//!
//! Read clauses run over a read-only [`GraphSource`]. Write clauses need a
//! graph they can mutate *and* whose effects later clauses in the same
//! query observe (openCypher: writes are visible downstream). [`MutableGraph`]
//! provides both: it wraps an immutable base graph with an overlay of
//! pending changes and itself implements [`GraphSource`], so a `MATCH`
//! after a `CREATE` sees the created elements.
//!
//! Every applied change is also appended to an ordered [`Mutation`] log.
//! mex.1 proves the semantics against the in-memory overlay; mex.2 replays
//! the log into acetone-graph `put_node`/`put_edge` to persist into
//! workspace roots. Keeping the log ordered and deterministic is what lets
//! that replay reproduce identical prolly-tree roots (Load-Bearing
//! Invariant #1).

use std::collections::BTreeMap;

use crate::ast::Direction;
use crate::exec::source::GraphSource;
use crate::exec::value::{EntityId, NodeValue, RelValue, Value};

/// One graph change, recorded in application order. The variants beyond
/// create arrive with the SET/REMOVE (acetone-eah) and DELETE
/// (acetone-921) beads.
#[derive(Debug, Clone)]
pub enum Mutation {
    CreateNode(NodeValue),
    CreateRel(RelValue),
}

/// The cumulative side effects of a write query — the openCypher
/// "side effects" the TCK verifies, and what the CLI reports after a write
/// (`Created 2 nodes, 1 relationship`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteSummary {
    pub nodes_created: u64,
    pub relationships_created: u64,
    pub properties_set: u64,
    pub labels_added: u64,
    pub nodes_deleted: u64,
    pub relationships_deleted: u64,
}

impl WriteSummary {
    /// Whether any change was recorded (drives "nothing to commit").
    pub fn is_empty(&self) -> bool {
        *self == WriteSummary::default()
    }
}

/// A read-only base graph plus an overlay of pending creates. Reads merge
/// the two; writes append to the overlay and the mutation log.
///
/// Created relationships may connect base nodes, created nodes, or a mix;
/// `expand` therefore scans both the base's edges and the overlay's.
pub struct MutableGraph<'a> {
    base: &'a dyn GraphSource,
    created_nodes: Vec<NodeValue>,
    created_rels: Vec<RelValue>,
    log: Vec<Mutation>,
    summary: WriteSummary,
    /// Monotonic counter for synthesising overlay identities. Shared
    /// across nodes and relationships so no two overlay elements collide.
    next_id: u64,
}

impl<'a> MutableGraph<'a> {
    pub fn new(base: &'a dyn GraphSource) -> Self {
        MutableGraph {
            base,
            created_nodes: Vec::new(),
            created_rels: Vec::new(),
            log: Vec::new(),
            summary: WriteSummary::default(),
            next_id: 0,
        }
    }

    fn fresh_id(&mut self) -> EntityId {
        let id = EntityId::from_bytes(format!("w{}", self.next_id).into_bytes());
        self.next_id += 1;
        id
    }

    /// Create a node with `labels` and `properties`; returns the new node
    /// value (its identity is freshly synthesised for this query).
    pub fn create_node(
        &mut self,
        labels: Vec<String>,
        properties: BTreeMap<String, Value>,
    ) -> NodeValue {
        let node = NodeValue {
            id: self.fresh_id(),
            labels,
            properties,
        };
        self.created_nodes.push(node.clone());
        self.log.push(Mutation::CreateNode(node.clone()));
        self.summary.nodes_created += 1;
        node
    }

    /// Create a relationship of `rel_type` from `start` to `end`.
    pub fn create_rel(
        &mut self,
        start: EntityId,
        rel_type: String,
        end: EntityId,
        properties: BTreeMap<String, Value>,
    ) -> RelValue {
        let rel = RelValue {
            id: self.fresh_id(),
            rel_type,
            start,
            end,
            properties,
        };
        self.created_rels.push(rel.clone());
        self.log.push(Mutation::CreateRel(rel.clone()));
        self.summary.relationships_created += 1;
        rel
    }

    pub fn summary(&self) -> &WriteSummary {
        &self.summary
    }

    /// Consume the graph, yielding the ordered mutation log and the summary
    /// for persistence (mex.2).
    pub fn into_log(self) -> (Vec<Mutation>, WriteSummary) {
        (self.log, self.summary)
    }
}

impl GraphSource for MutableGraph<'_> {
    fn all_nodes(&self) -> Vec<NodeValue> {
        let mut nodes = self.base.all_nodes();
        nodes.extend(self.created_nodes.iter().cloned());
        nodes
    }

    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        let mut out = self.base.expand(node, direction, types);
        for rel in &self.created_rels {
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
        self.created_nodes
            .iter()
            .find(|n| n.id == *id)
            .cloned()
            .or_else(|| self.base.node(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::source::{EmptyGraph, MemoryGraph};

    #[test]
    fn overlay_merges_reads_over_the_base() {
        let mut base = MemoryGraph::new();
        base.add_node(["Host"], BTreeMap::new());

        let mut graph = MutableGraph::new(&base);
        assert_eq!(graph.all_nodes().len(), 1);

        let created = graph.create_node(vec!["Host".into()], BTreeMap::new());
        // Both base and overlay nodes are visible; the new one resolves.
        assert_eq!(graph.all_nodes().len(), 2);
        assert!(graph.node(&created.id).is_some());
        assert_eq!(graph.summary().nodes_created, 1);
    }

    #[test]
    fn created_relationships_expand_from_created_nodes() {
        let empty = EmptyGraph;
        let mut graph = MutableGraph::new(&empty);
        let a = graph.create_node(vec!["A".into()], BTreeMap::new());
        let b = graph.create_node(vec!["B".into()], BTreeMap::new());
        graph.create_rel(a.id.clone(), "R".into(), b.id.clone(), BTreeMap::new());

        let out = graph.expand(&a.id, Direction::Out, &["R".to_string()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.id, b.id);
        // No incoming R from a's perspective.
        assert!(graph.expand(&a.id, Direction::In, &[]).is_empty());

        let (log, summary) = graph.into_log();
        assert_eq!(log.len(), 3); // two nodes, one rel
        assert_eq!(summary.nodes_created, 2);
        assert_eq!(summary.relationships_created, 1);
    }
}
