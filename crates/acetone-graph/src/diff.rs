//! Graph-level diff (spec §7; Phase 4, acetone-14c.1).
//!
//! Classifies the structural change between two graph versions into node
//! and edge added/removed/modified records. It is built on the prolly map
//! diff ([`acetone_prolly::diff`]), which streams the changed
//! `(key, before, after)` triples of one map in key order and — by content
//! addressing — skips every shared subtree, so the cost is proportional to
//! what changed, not to graph size.
//!
//! A diff is a **derived view**: reproducible from the two versions and
//! never stored, so it carries no `format_version`. The classification is
//! deterministic (elements in ascending key order), which the merge and
//! blame beads (acetone-14c.2/.6) and the `_Added`/`_Removed`/`_Modified`
//! virtual graph build on.

use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};

/// How an element changed from the `from` version to the `to` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Present only in `to`.
    Added,
    /// Present only in `from`.
    Removed,
    /// Present in both, with a different record.
    Modified,
}

impl ChangeKind {
    /// The virtual label this change contributes to the diff graph
    /// (`MATCH (n:_Added) …`).
    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Added => "_Added",
            ChangeKind::Removed => "_Removed",
            ChangeKind::Modified => "_Modified",
        }
    }
}

/// One node's change. `before`/`after` are its records in the `from`/`to`
/// versions; the key's own properties are re-exposed from the key by the
/// graph layer and are not part of the record (ADR-0008), so a key change
/// is an `Added`+`Removed` pair, never a `Modified` (Invariant #3).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeChange {
    /// How the node changed.
    pub kind: ChangeKind,
    /// The node's identity `(primary label, key tuple)`.
    pub key: NodeKey,
    /// The record in the `from` version (`None` when `Added`).
    pub before: Option<NodeRecord>,
    /// The record in the `to` version (`None` when `Removed`).
    pub after: Option<NodeRecord>,
}

/// One relationship's change (from the forward edge map).
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeChange {
    /// How the relationship changed.
    pub kind: ChangeKind,
    /// The relationship's key `(src, type, dst, discriminator)`.
    pub key: EdgeKey,
    /// The record in the `from` version (`None` when `Added`).
    pub before: Option<EdgeRecord>,
    /// The record in the `to` version (`None` when `Removed`).
    pub after: Option<EdgeRecord>,
}

/// The classified difference between two graph versions: node changes then
/// edge changes, each in ascending key order (deterministic). The reverse
/// edge map is derived from the forward map and is not diffed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphDiff {
    /// Node changes, in ascending node-key order.
    pub nodes: Vec<NodeChange>,
    /// Edge changes, in ascending forward-edge-key order.
    pub edges: Vec<EdgeChange>,
}

impl GraphDiff {
    /// True when the two versions have identical graph content.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }
}

/// Classify one map-diff triple by which sides are present. A prolly diff
/// never emits a triple whose values are equal, so "present in both" is
/// always a genuine `Modified`.
pub(crate) fn classify(before_present: bool, after_present: bool) -> ChangeKind {
    match (before_present, after_present) {
        (false, true) => ChangeKind::Added,
        (true, false) => ChangeKind::Removed,
        (true, true) => ChangeKind::Modified,
        // The prolly diff never emits a key absent on both sides.
        (false, false) => unreachable!("prolly diff emits only changed keys"),
    }
}
