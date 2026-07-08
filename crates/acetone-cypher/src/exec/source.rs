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

    /// Nodes an equality on the declared index `index_name` selects for
    /// `value` (`IndexSeek`, spec §5.3). `None` means this source has no such
    /// index, so the caller falls back to a label scan; `Some` is a candidate
    /// superset the caller still filters. Indexes are null/NaN-blind, so a
    /// null or NaN `value` selects nothing. The default has no indexes.
    fn nodes_by_index(&self, _index_name: &str, _value: &Value) -> Option<Vec<NodeValue>> {
        None
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

/// Runs `CALL acetone.*` procedures (spec §5.2). The executor knows the
/// procedure catalogue (arity, yield columns) but not how to compute the
/// results — those need the repository (history, diff, blame, conflicts),
/// which lives above this crate. The workbench path (acetone-cli) supplies
/// a provider over `Repository`; pure-executor callers (tests, TCK) use
/// [`NoProcedures`].
pub trait ProcedureProvider {
    /// Run procedure `name` (already existence- and arity-checked by the
    /// binder) with evaluated `args`. Returns one row per result, each a
    /// full tuple aligned to the procedure's declared yield columns (in
    /// declaration order). `Err(message)` for a bad argument or a procedure
    /// this provider cannot serve.
    ///
    /// **Contract:** every returned tuple must have exactly one value per
    /// declared yield column, in declaration order. The executor binds
    /// columns by position; a short tuple would silently yield nulls and a
    /// long one would leak extra cells, so a provider that cannot honour the
    /// shape should return `Err` rather than a mis-sized tuple. (Debug builds
    /// assert the width.)
    fn call(&self, name: &str, args: &[Value]) -> Result<Vec<Vec<Value>>, String>;
}

/// A provider that serves no procedures — every `CALL` is an error. Keeps
/// pure-executor callers (tests, the TCK backend) working without a
/// repository behind them.
pub struct NoProcedures;

impl ProcedureProvider for NoProcedures {
    fn call(&self, name: &str, _args: &[Value]) -> Result<Vec<Vec<Value>>, String> {
        Err(format!(
            "procedure {name} needs a repository-backed provider; this query has none"
        ))
    }
}

/// Resolves the graph a clause queries: the base (checked-out) version,
/// or the version at a refspec for a clause-group `AT` (spec §5.2).
/// Distinct `MATCH ... AT <ref>` clauses in one query may address distinct
/// versions, so `at` yields an owned source the caller holds for the
/// clause's duration.
pub trait VersionResolver {
    /// The base version — the graph a `MATCH` with no `AT` queries.
    fn base(&self) -> &dyn GraphSource;

    /// The graph at `refspec`. Errs (message only) if the ref cannot be
    /// resolved or this resolver has no repository behind it.
    fn at(&self, refspec: &str) -> Result<Box<dyn GraphSource>, String>;
}

/// Wraps a single graph as a resolver: `AT` is unsupported (no repository
/// behind it). Keeps pure-executor callers (tests, TCK backend) working
/// without ref-resolution plumbing.
pub struct SingleVersion<'a> {
    graph: &'a dyn GraphSource,
}

impl<'a> SingleVersion<'a> {
    pub fn new(graph: &'a dyn GraphSource) -> Self {
        SingleVersion { graph }
    }
}

impl VersionResolver for SingleVersion<'_> {
    fn base(&self) -> &dyn GraphSource {
        self.graph
    }

    fn at(&self, refspec: &str) -> Result<Box<dyn GraphSource>, String> {
        Err(format!(
            "AT '{refspec}' needs a repository-backed resolver; this query has a single fixed graph"
        ))
    }
}

/// A simple in-memory property graph.
#[derive(Debug, Default)]
pub struct MemoryGraph {
    nodes: Vec<NodeValue>,
    rels: Vec<RelValue>,
    /// Monotonic source of stable ids for elements folded in by [`apply`]
    /// (see there); never reused, so successive applies cannot collide.
    next_id: u64,
}

impl MemoryGraph {
    pub fn new() -> Self {
        MemoryGraph::default()
    }

    /// Fold a query's net [`WriteChanges`](crate::exec::write::WriteChanges)
    /// into this graph, so a following query sees them. This is the TCK
    /// harness's setup-graph accumulator (acetone-1h7): a scenario's setup
    /// queries build the base graph one statement at a time.
    ///
    /// Created elements carry overlay ids (first byte `0xFF`) whose counter
    /// restarts each query, so they are remapped to fresh stable ids before
    /// being stored; base ids (matched-and-modified elements, deletions)
    /// pass through unchanged. Deletions apply before upserts, matching the
    /// persistence layer's ordering.
    pub fn apply(&mut self, changes: &crate::exec::write::WriteChanges) {
        use std::collections::{HashMap, HashSet};

        let deleted: HashSet<EntityId> = changes.deleted_nodes.iter().cloned().collect();
        self.nodes.retain(|n| !deleted.contains(&n.id));
        let deleted_rels: HashSet<EntityId> =
            changes.deleted_rels.iter().map(|r| r.id.clone()).collect();
        self.rels.retain(|r| !deleted_rels.contains(&r.id));

        let mut remap: HashMap<EntityId, EntityId> = HashMap::new();
        let mut stable = |old: &EntityId, next: &mut u64| -> EntityId {
            if !is_overlay(old) {
                return old.clone();
            }
            remap
                .entry(old.clone())
                .or_insert_with(|| {
                    let id = EntityId::from_bytes(format!("s{next}").into_bytes());
                    *next += 1;
                    id
                })
                .clone()
        };

        for node in &changes.upserted_nodes {
            let id = stable(&node.id, &mut self.next_id);
            let value = NodeValue {
                id: id.clone(),
                labels: node.labels.clone(),
                properties: node.properties.clone(),
            };
            match self.nodes.iter_mut().find(|n| n.id == id) {
                Some(slot) => *slot = value,
                None => self.nodes.push(value),
            }
        }
        for rel in &changes.upserted_rels {
            let id = stable(&rel.id, &mut self.next_id);
            let value = RelValue {
                id: id.clone(),
                rel_type: rel.rel_type.clone(),
                start: stable(&rel.start, &mut self.next_id),
                end: stable(&rel.end, &mut self.next_id),
                properties: rel.properties.clone(),
            };
            match self.rels.iter_mut().find(|r| r.id == id) {
                Some(slot) => *slot = value,
                None => self.rels.push(value),
            }
        }
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

impl MemoryGraph {
    /// Every relationship, in a stable order. The [`GraphSource`] trait
    /// exposes relationships only by expansion; the TCK side-effect diff
    /// (acetone-1h7) needs the whole set.
    pub fn all_rels(&self) -> Vec<RelValue> {
        self.rels.clone()
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

/// True when `id` is an executor overlay id — a created element's id,
/// carrying the `0xFF` tag byte reserved from the memcomparable key space
/// (acetone-j5m). Base-graph ids never start with it.
fn is_overlay(id: &EntityId) -> bool {
    id.0.first() == Some(&0xFF)
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
