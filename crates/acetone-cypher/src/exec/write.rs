//! The write-execution model (spec §5.1 Level W, Phase 3).
//!
//! Read clauses run over a read-only [`GraphSource`]. Write clauses need a
//! graph they can mutate *and* whose effects later clauses in the same
//! query observe (openCypher: writes are visible downstream). [`MutableGraph`]
//! provides both: it wraps an immutable base graph with an overlay of
//! pending changes and itself implements [`GraphSource`], so a `MATCH`
//! after a `CREATE` sees the created elements.
//!
//! Every applied change is also appended to an ordered [`Mutation`] log
//! (kept for audit/determinism). For persistence, [`MutableGraph::changes`]
//! summarises the query's *net effect* as final entity states — the shape
//! `crate::persist` replays into acetone-graph `put_node`/`put_edge`/
//! `delete_*` (mex.2); persisting final states, not each granular op,
//! sidesteps read-modify-write for `SET`. History independence itself
//! (Load-Bearing Invariant #1 — identical final contents yield identical
//! roots regardless of order) is provided by the prolly-tree layer.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::ast::Direction;
use crate::exec::source::GraphSource;
use crate::exec::value::{EntityId, NodeValue, RelValue, Value};

/// Reserved first byte for synthesised overlay identities. Chosen to sit
/// above the memcomparable key encoder's whole type-tag space (0x00..=0x0c),
/// so an overlay id can never equal a storage-derived key (acetone-j5m).
const OVERLAY_ID_TAG: u8 = 0xFF;

/// One graph change, recorded in application order. The variants beyond
/// create arrive with the SET/REMOVE (acetone-eah) and DELETE
/// (acetone-921) beads. A `value` of `None` on a property mutation is a
/// removal (openCypher: `SET x.p = null` and `REMOVE x.p` both delete).
#[derive(Debug, Clone)]
pub enum Mutation {
    CreateNode(NodeValue),
    CreateRel(RelValue),
    SetNodeProperty {
        id: EntityId,
        key: String,
        value: Option<Value>,
    },
    ReplaceNodeProperties {
        id: EntityId,
        properties: BTreeMap<String, Value>,
    },
    AddNodeLabel {
        id: EntityId,
        label: String,
    },
    RemoveNodeLabel {
        id: EntityId,
        label: String,
    },
    SetRelProperty {
        id: EntityId,
        key: String,
        value: Option<Value>,
    },
    ReplaceRelProperties {
        id: EntityId,
        properties: BTreeMap<String, Value>,
    },
    DeleteNode {
        id: EntityId,
    },
    DeleteRel {
        id: EntityId,
    },
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
    pub labels_removed: u64,
    pub nodes_deleted: u64,
    pub relationships_deleted: u64,
}

impl WriteSummary {
    /// Whether any change was recorded (drives "nothing to commit").
    pub fn is_empty(&self) -> bool {
        *self == WriteSummary::default()
    }
}

/// The net effect of a write query on the base graph, as final entity
/// states rather than a granular operation log — the shape a persistence
/// layer (acetone-mex.2) replays into `put_node`/`put_edge`/`delete_*`.
/// Persisting final states sidesteps read-modify-write for `SET` (the
/// upserted node already carries its final properties).
#[derive(Debug, Default)]
pub struct WriteChanges {
    /// Created or modified nodes, in their final state (deletions excluded).
    pub upserted_nodes: Vec<NodeValue>,
    /// Base nodes to delete. Created-then-deleted nodes are a net no-op and
    /// are omitted (their overlay id encodes no persistent identity).
    pub deleted_nodes: Vec<EntityId>,
    /// Created or modified relationships, in their final state.
    pub upserted_rels: Vec<RelValue>,
    /// Relationships to delete, carrying endpoints and type so the edge key
    /// can be rebuilt.
    pub deleted_rels: Vec<RelValue>,
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
    /// Current state of any mutated node/relationship, keyed by identity —
    /// covers base *and* created entities (a created entity mutated later
    /// gets an override entry that shadows its `created_*` value). Reads
    /// consult these first.
    node_overrides: HashMap<EntityId, NodeValue>,
    rel_overrides: HashMap<EntityId, RelValue>,
    /// Entities deleted this query. All reads exclude them, so a deleted
    /// node/relationship is invisible to later clauses. Deleted
    /// relationships keep their value so persistence can rebuild the edge
    /// key (the id alone does not encode the endpoints).
    deleted_nodes: HashSet<EntityId>,
    deleted_rels: HashMap<EntityId, RelValue>,
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
            node_overrides: HashMap::new(),
            rel_overrides: HashMap::new(),
            deleted_nodes: HashSet::new(),
            deleted_rels: HashMap::new(),
            log: Vec::new(),
            summary: WriteSummary::default(),
            next_id: 0,
        }
    }

    fn fresh_id(&mut self) -> EntityId {
        // A reserved sentinel first byte makes overlay identities
        // structurally disjoint from any storage-derived node key: the
        // memcomparable key encoder (acetone-model `keys.rs`) begins every
        // key with a type tag in 0x01..=0x0c, so nothing it produces can
        // start with `OVERLAY_ID_TAG`. This holds whatever base graph backs
        // the overlay — an in-memory `MemoryGraph` or a storage snapshot —
        // so a created node can never alias an existing one (acetone-j5m).
        let mut bytes = Vec::with_capacity(1 + std::mem::size_of::<u64>());
        bytes.push(OVERLAY_ID_TAG);
        bytes.extend_from_slice(&self.next_id.to_be_bytes());
        self.next_id += 1;
        EntityId::from_bytes(bytes)
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

    // --- SET / REMOVE (acetone-eah) -------------------------------------

    /// Current value of node `id`, overlay-aware: an override wins, then a
    /// created node, then the base graph. A deleted node is gone.
    pub fn current_node(&self, id: &EntityId) -> Option<NodeValue> {
        if self.deleted_nodes.contains(id) {
            return None;
        }
        self.node_overrides
            .get(id)
            .cloned()
            .or_else(|| self.created_nodes.iter().find(|n| n.id == *id).cloned())
            .or_else(|| self.base.node(id))
    }

    /// The current value of a relationship the caller already holds
    /// (relationships have no by-id base lookup; the executor carries the
    /// row's `RelValue`), overlay-aware.
    pub fn current_rel(&self, rel: &RelValue) -> RelValue {
        self.rel_overrides.get(&rel.id).cloned().unwrap_or_else(|| {
            self.created_rels
                .iter()
                .find(|r| r.id == rel.id)
                .cloned()
                .unwrap_or_else(|| rel.clone())
        })
    }

    fn store_node(&mut self, node: NodeValue) {
        self.node_overrides.insert(node.id.clone(), node);
    }

    /// Set (or, with `None`, remove) a node property. Returns the updated
    /// node, or `None` if `id` names no live node.
    pub fn set_node_property(
        &mut self,
        id: &EntityId,
        key: String,
        value: Option<Value>,
    ) -> Option<NodeValue> {
        let mut node = self.current_node(id)?;
        match &value {
            Some(v) => {
                node.properties.insert(key.clone(), v.clone());
            }
            None => {
                node.properties.remove(&key);
            }
        }
        self.log.push(Mutation::SetNodeProperty {
            id: id.clone(),
            key,
            value,
        });
        self.summary.properties_set += 1;
        self.store_node(node.clone());
        Some(node)
    }

    /// Replace (`SET n = {..}`) or merge (`SET n += {..}`) a node's whole
    /// property map. Merge keeps unlisted properties; replace drops them.
    /// A `null` value in the map removes that key.
    pub fn set_node_properties(
        &mut self,
        id: &EntityId,
        properties: BTreeMap<String, Value>,
        merge: bool,
    ) -> Option<NodeValue> {
        let mut node = self.current_node(id)?;
        if !merge {
            node.properties.clear();
        }
        let mut written = 0u64;
        for (key, value) in &properties {
            if value.is_null() {
                node.properties.remove(key);
            } else {
                node.properties.insert(key.clone(), value.clone());
            }
            written += 1;
        }
        self.log.push(Mutation::ReplaceNodeProperties {
            id: id.clone(),
            properties: node.properties.clone(),
        });
        self.summary.properties_set += written;
        self.store_node(node.clone());
        Some(node)
    }

    /// Add a label to a node (idempotent). Returns the updated node.
    pub fn add_node_label(&mut self, id: &EntityId, label: String) -> Option<NodeValue> {
        let mut node = self.current_node(id)?;
        if !node.labels.contains(&label) {
            node.labels.push(label.clone());
            self.log.push(Mutation::AddNodeLabel {
                id: id.clone(),
                label,
            });
            self.summary.labels_added += 1;
            self.store_node(node.clone());
        }
        Some(node)
    }

    /// Remove a label from a node (idempotent). Returns the updated node.
    pub fn remove_node_label(&mut self, id: &EntityId, label: &str) -> Option<NodeValue> {
        let mut node = self.current_node(id)?;
        if let Some(pos) = node.labels.iter().position(|l| l == label) {
            node.labels.remove(pos);
            self.log.push(Mutation::RemoveNodeLabel {
                id: id.clone(),
                label: label.to_string(),
            });
            self.summary.labels_removed += 1;
            self.store_node(node.clone());
        }
        Some(node)
    }

    fn store_rel(&mut self, rel: RelValue) {
        self.rel_overrides.insert(rel.id.clone(), rel);
    }

    /// Set (or remove) a relationship property. `current` is the caller's
    /// held value; the graph reconciles it with any prior override.
    pub fn set_rel_property(
        &mut self,
        current: &RelValue,
        key: String,
        value: Option<Value>,
    ) -> RelValue {
        let mut rel = self.current_rel(current);
        match &value {
            Some(v) => {
                rel.properties.insert(key.clone(), v.clone());
            }
            None => {
                rel.properties.remove(&key);
            }
        }
        self.log.push(Mutation::SetRelProperty {
            id: rel.id.clone(),
            key,
            value,
        });
        self.summary.properties_set += 1;
        self.store_rel(rel.clone());
        rel
    }

    /// Replace or merge a relationship's whole property map.
    pub fn set_rel_properties(
        &mut self,
        current: &RelValue,
        properties: BTreeMap<String, Value>,
        merge: bool,
    ) -> RelValue {
        let mut rel = self.current_rel(current);
        if !merge {
            rel.properties.clear();
        }
        let mut written = 0u64;
        for (key, value) in &properties {
            if value.is_null() {
                rel.properties.remove(key);
            } else {
                rel.properties.insert(key.clone(), value.clone());
            }
            written += 1;
        }
        self.log.push(Mutation::ReplaceRelProperties {
            id: rel.id.clone(),
            properties: rel.properties.clone(),
        });
        self.summary.properties_set += written;
        self.store_rel(rel.clone());
        rel
    }

    /// A node's overriding value *only if it was mutated this query* — no
    /// fallback to the created set or base. Used to refresh row bindings
    /// after a write without disturbing values that were never mutated,
    /// notably `AT <ref>` snapshots that share a base node's identity but
    /// carry a different version's properties (Invariant #3).
    pub fn node_override(&self, id: &EntityId) -> Option<NodeValue> {
        self.node_overrides.get(id).cloned()
    }

    /// A relationship's current value if it has been mutated this query.
    pub fn rel_override(&self, id: &EntityId) -> Option<RelValue> {
        self.rel_overrides.get(id).cloned()
    }

    // --- DELETE / DETACH DELETE (acetone-921) ---------------------------

    /// Whether `id` currently has any incident relationship (either
    /// direction, any type), overlay- and deletion-aware. Drives the
    /// "cannot delete a connected node without DETACH" rule.
    pub fn has_incident_rels(&self, id: &EntityId) -> bool {
        !self.expand(id, Direction::Undirected, &[]).is_empty()
    }

    /// Delete a relationship (idempotent). Excluded from all later reads;
    /// its value is retained so persistence can rebuild the edge key.
    pub fn delete_rel(&mut self, rel: &RelValue) {
        if !self.deleted_rels.contains_key(&rel.id) {
            self.deleted_rels.insert(rel.id.clone(), rel.clone());
            self.log.push(Mutation::DeleteRel { id: rel.id.clone() });
            self.summary.relationships_deleted += 1;
        }
    }

    /// Delete a node (idempotent). The caller guarantees it has no incident
    /// relationships (plain DELETE) or has already detached them.
    pub fn delete_node(&mut self, id: &EntityId) {
        if self.deleted_nodes.insert(id.clone()) {
            self.log.push(Mutation::DeleteNode { id: id.clone() });
            self.summary.nodes_deleted += 1;
        }
    }

    /// Remove every relationship incident to `id`, then the node itself
    /// (DETACH DELETE).
    pub fn detach_delete_node(&mut self, id: &EntityId) {
        let incident: Vec<RelValue> = self
            .expand(id, Direction::Undirected, &[])
            .into_iter()
            .map(|(rel, _)| rel)
            .collect();
        for rel in &incident {
            self.delete_rel(rel);
        }
        self.delete_node(id);
    }

    pub fn summary(&self) -> &WriteSummary {
        &self.summary
    }

    /// Consume the graph, yielding the ordered mutation log and the summary.
    pub fn into_log(self) -> (Vec<Mutation>, WriteSummary) {
        (self.log, self.summary)
    }

    /// The net effect of this query as final entity states (acetone-mex.2).
    /// A created node that was also deleted, or a base node/relationship
    /// only read, contributes nothing.
    pub fn changes(&self) -> WriteChanges {
        let created_node_ids: HashSet<&EntityId> =
            self.created_nodes.iter().map(|n| &n.id).collect();
        let created_rel_ids: HashSet<&EntityId> = self.created_rels.iter().map(|r| &r.id).collect();

        let mut seen = HashSet::new();
        let mut upserted_nodes = Vec::new();
        // Created nodes first (final state via any later override)...
        for node in &self.created_nodes {
            if self.deleted_nodes.contains(&node.id) {
                continue;
            }
            if seen.insert(node.id.clone())
                && let Some(current) = self.current_node(&node.id)
            {
                upserted_nodes.push(current);
            }
        }
        // ...then modified base nodes (overrides not already covered).
        for (id, node) in &self.node_overrides {
            if self.deleted_nodes.contains(id) {
                continue;
            }
            if seen.insert(id.clone()) {
                upserted_nodes.push(node.clone());
            }
        }

        // A deleted base node has a decodable storage identity; a deleted
        // created node never persisted, so omit it.
        let deleted_nodes = self
            .deleted_nodes
            .iter()
            .filter(|id| !created_node_ids.contains(id))
            .cloned()
            .collect();

        let mut seen_rels = HashSet::new();
        let mut upserted_rels = Vec::new();
        for rel in &self.created_rels {
            if self.deleted_rels.contains_key(&rel.id) {
                continue;
            }
            if seen_rels.insert(rel.id.clone()) {
                upserted_rels.push(self.overlay_rel(rel.clone()));
            }
        }
        for (id, rel) in &self.rel_overrides {
            if self.deleted_rels.contains_key(id) {
                continue;
            }
            if seen_rels.insert(id.clone()) {
                upserted_rels.push(rel.clone());
            }
        }

        let deleted_rels = self
            .deleted_rels
            .iter()
            .filter(|(id, _)| !created_rel_ids.contains(id))
            .map(|(_, rel)| rel.clone())
            .collect();

        WriteChanges {
            upserted_nodes,
            deleted_nodes,
            upserted_rels,
            deleted_rels,
        }
    }
}

impl MutableGraph<'_> {
    /// Apply any recorded property/label override to a node value read from
    /// the base or created set.
    fn overlay_node(&self, node: NodeValue) -> NodeValue {
        self.node_overrides.get(&node.id).cloned().unwrap_or(node)
    }

    /// Apply any recorded property override to a relationship value.
    fn overlay_rel(&self, rel: RelValue) -> RelValue {
        self.rel_overrides.get(&rel.id).cloned().unwrap_or(rel)
    }
}

impl GraphSource for MutableGraph<'_> {
    fn all_nodes(&self) -> Vec<NodeValue> {
        let mut nodes: Vec<NodeValue> = self
            .base
            .all_nodes()
            .into_iter()
            .filter(|n| !self.deleted_nodes.contains(&n.id))
            .map(|n| self.overlay_node(n))
            .collect();
        nodes.extend(
            self.created_nodes
                .iter()
                .filter(|n| !self.deleted_nodes.contains(&n.id))
                .map(|n| self.overlay_node(n.clone())),
        );
        nodes
    }

    fn expand(
        &self,
        node: &EntityId,
        direction: Direction,
        types: &[String],
    ) -> Vec<(RelValue, NodeValue)> {
        // A deleted anchor has no edges.
        if self.deleted_nodes.contains(node) {
            return Vec::new();
        }
        // Base edges, with any property/label overrides applied to both the
        // relationship and its neighbour; deleted edges and edges to deleted
        // neighbours are excluded.
        let mut out: Vec<(RelValue, NodeValue)> = self
            .base
            .expand(node, direction, types)
            .into_iter()
            .filter(|(rel, neighbour)| {
                !self.deleted_rels.contains_key(&rel.id)
                    && !self.deleted_nodes.contains(&neighbour.id)
            })
            .map(|(rel, neighbour)| (self.overlay_rel(rel), self.overlay_node(neighbour)))
            .collect();
        for rel in &self.created_rels {
            if self.deleted_rels.contains_key(&rel.id) {
                continue;
            }
            let rel = self.overlay_rel(rel.clone());
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
            // `node()` already excludes deleted neighbours.
            if let Some(neighbour) = self.node(neighbour) {
                out.push((rel.clone(), neighbour));
            }
        }
        out
    }

    fn node(&self, id: &EntityId) -> Option<NodeValue> {
        self.current_node(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::source::{EmptyGraph, MemoryGraph};

    #[test]
    fn overlay_ids_cannot_collide_with_storage_keys() {
        // Overlay ids start with the reserved sentinel...
        let empty = EmptyGraph;
        let mut graph = MutableGraph::new(&empty);
        let a = graph.create_node(vec!["A".into()], BTreeMap::new());
        let b = graph.create_node(vec!["A".into()], BTreeMap::new());
        assert_eq!(a.id.0[0], OVERLAY_ID_TAG);
        assert_ne!(a.id, b.id, "overlay ids must be unique");

        // ...and the memcomparable key encoder never emits that first byte,
        // so no storage-derived node key can equal an overlay id. Exercise
        // a spread of key shapes a real node identity might take.
        use acetone_model::Value as MValue;
        for tuple in [
            vec![
                MValue::String("Host".into()),
                MValue::String("web-01".into()),
            ],
            vec![MValue::String("N".into()), MValue::Int(0)],
            vec![MValue::Int(i64::MAX)],
            vec![MValue::List(vec![MValue::String("x".into())])],
        ] {
            let encoded = acetone_model::keys::encode_key(&tuple).unwrap();
            assert_ne!(
                encoded[0], OVERLAY_ID_TAG,
                "encoder must never start a key with the overlay sentinel"
            );
        }
    }

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
