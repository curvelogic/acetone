//! Declared-constraint validation shared by the bulk enforcement points
//! (acetone-9gw): import, `declare-label` backfill, and fsck's advisory pass.
//!
//! The Cypher write path enforces existence and UNIQUE per upserted node
//! (`acetone-cypher::persist`); merge re-validates changed keys
//! (`merge::validate_merged`). This module carries the same semantics for
//! whole-node-set callers:
//!
//! - **existence**: a required property is present iff it is a key property
//!   (always present, by identity) or appears in the node record;
//! - **UNIQUE**: two distinct nodes of the same label sharing a canonical
//!   value encoding for a UNIQUE property collide. Node identity is the
//!   encoded node key, so re-asserting the same node with the same value is
//!   never a self-collision.
//!
//! Only node constraints are checked, matching the write path: relationship
//! existence constraints have no declaring surface in v0.1 and are enforced
//! nowhere.
//!
//! Everything here is a pure read over its inputs — no encoding, record or
//! commit bytes change for valid data — and the violation order is
//! deterministic (existence in node-key order, then UNIQUE in
//! label/property/value order), so callers' error output is stable
//! (Invariant #4 discipline applied to error reporting).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use acetone_model::Value;
use acetone_model::display::{format_label, format_node_key, format_value};
use acetone_model::graph_keys::NodeKey;
use acetone_model::records::{NodeRecord, RecordEncodeError};
use acetone_model::schema::LabelDef;
use acetone_model::values::encode_value;

use crate::error::GraphError;
use crate::repo::Snapshot;

/// How many violations a rendered report shows before summarising the rest.
pub const REPORT_LIMIT: usize = 20;

/// One breach of a declared schema constraint (spec §2).
#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintViolation {
    /// A node lacks a property its label declares with `--require`.
    MissingRequired {
        /// The violating node.
        node: NodeKey,
        /// The absent required property.
        property: String,
    },
    /// Two or more nodes of `label` share a value for a UNIQUE property.
    Unique {
        /// The label declaring the constraint.
        label: String,
        /// The UNIQUE property.
        property: String,
        /// The shared value (of the first colliding node, for display).
        value: Value,
        /// The colliding nodes, in node-key order.
        nodes: Vec<NodeKey>,
    },
}

impl fmt::Display for ConstraintViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConstraintViolation::MissingRequired { node, property } => write!(
                f,
                "node {} is missing required property {}",
                format_node_key(node),
                format_label(property)
            ),
            ConstraintViolation::Unique {
                label,
                property,
                value,
                nodes,
            } => {
                let keys: Vec<String> = nodes.iter().map(format_node_key).collect();
                write!(
                    f,
                    "UNIQUE {}.{} value {} shared by {} nodes: {}",
                    format_label(label),
                    format_label(property),
                    format_value(value),
                    nodes.len(),
                    keys.join(", ")
                )
            }
        }
    }
}

/// A deterministic, non-empty-or-not violation list with bounded rendering:
/// the first [`REPORT_LIMIT`] violations followed by a remainder count.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintViolations(pub Vec<ConstraintViolation>);

impl ConstraintViolations {
    /// Whether there are no violations.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for ConstraintViolations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.0.len();
        write!(
            f,
            "{total} constraint violation{}:",
            if total == 1 { "" } else { "s" }
        )?;
        for violation in self.0.iter().take(REPORT_LIMIT) {
            write!(f, "\n  {violation}")?;
        }
        if total > REPORT_LIMIT {
            write!(f, "\n  … and {} more", total - REPORT_LIMIT)?;
        }
        Ok(())
    }
}

/// A node set keyed by encoded node key — the caller's would-be final state,
/// in deterministic (memcomparable) order.
pub type NodeSet = BTreeMap<Vec<u8>, (NodeKey, NodeRecord)>;

/// Check `nodes` (a final node state) against the label definitions in
/// `labels`, returning every violation in deterministic order: existence
/// breaches in node-key order first, then UNIQUE collisions in
/// (label, property, value) order.
///
/// With `focus` set, only violations *involving* a focus key are reported: an
/// existence breach on a focus node, or a UNIQUE group containing at least
/// one focus node. This lets import blame only the data it touched, leaving
/// pre-existing breaches to fsck's advisory.
pub fn check_nodes(
    labels: &BTreeMap<String, LabelDef>,
    nodes: &NodeSet,
    focus: Option<&BTreeSet<Vec<u8>>>,
) -> Result<Vec<ConstraintViolation>, GraphError> {
    let in_focus = |encoded: &Vec<u8>| focus.is_none_or(|f| f.contains(encoded));

    let mut violations = Vec::new();

    // Existence, in node-key order.
    for (encoded, (key, record)) in nodes {
        if !in_focus(encoded) {
            continue;
        }
        let Some(def) = labels.get(key.label()) else {
            continue;
        };
        for property in def.exists() {
            let present = def.key().iter().any(|k| k == property)
                || record.properties().contains_key(property);
            if !present {
                violations.push(ConstraintViolation::MissingRequired {
                    node: key.clone(),
                    property: property.clone(),
                });
            }
        }
    }

    // UNIQUE: group by (label, property, canonical value encoding) — the
    // same equality the merge validator uses — over the whole final state.
    #[allow(clippy::type_complexity)]
    let mut groups: BTreeMap<(String, String, Vec<u8>), (Value, Vec<(Vec<u8>, NodeKey)>)> =
        BTreeMap::new();
    for (encoded, (key, record)) in nodes {
        let Some(def) = labels.get(key.label()) else {
            continue;
        };
        for property in def.unique() {
            if let Some(value) = record.properties().get(property) {
                let value_enc = encode_value(value).map_err(RecordEncodeError::from)?;
                groups
                    .entry((key.label().to_owned(), property.clone(), value_enc))
                    .or_insert_with(|| (value.clone(), Vec::new()))
                    .1
                    .push((encoded.clone(), key.clone()));
            }
        }
    }
    for ((label, property, _), (value, members)) in groups {
        if members.len() < 2 || !members.iter().any(|(encoded, _)| in_focus(encoded)) {
            continue;
        }
        violations.push(ConstraintViolation::Unique {
            label,
            property,
            value,
            nodes: members.into_iter().map(|(_, key)| key).collect(),
        });
    }

    Ok(violations)
}

/// Check a single would-be node upsert against `snapshot`'s schema and
/// nodes — the guard for plumbing writes (`acetone put-node`), mirroring the
/// import path's final-state check with a one-key focus: the write is judged
/// against the workspace as it would be *after* the put (so replacing a node
/// with itself is never a self-collision), and only violations involving the
/// written key are reported (a pre-existing breach elsewhere is fsck's
/// business, not this write's).
pub fn check_upsert(
    snapshot: &Snapshot<'_>,
    key: &NodeKey,
    record: &NodeRecord,
) -> Result<Vec<ConstraintViolation>, GraphError> {
    let mut labels = BTreeMap::new();
    for entry in snapshot.schema_entries()? {
        if let acetone_model::schema::SchemaEntry::Label { name, def } = entry {
            labels.insert(name, def);
        }
    }
    // Fast path: an undeclared or unconstrained label has nothing to check —
    // plumbing writes to schema-less labels stay raw, like `put_node` itself.
    match labels.get(key.label()) {
        Some(def) if !def.exists().is_empty() || !def.unique().is_empty() => {}
        _ => return Ok(Vec::new()),
    }
    let mut nodes = NodeSet::new();
    for (k, r) in snapshot.nodes()? {
        nodes.insert(k.encode()?, (k, r));
    }
    let encoded = key.encode()?;
    let focus: BTreeSet<Vec<u8>> = [encoded.clone()].into_iter().collect();
    nodes.insert(encoded, (key.clone(), record.clone()));
    check_nodes(&labels, &nodes, Some(&focus))
}

/// Check every existing node bearing `label` against `def` — the backfill
/// check run when a label is (re)declared over existing data, closing the
/// silent-retrofit gap: a `--require`/`--unique` set the data already
/// violates is refused with the violating keys named, instead of accepted
/// and left to fail unrelated writes later.
pub fn check_label(
    snapshot: &Snapshot<'_>,
    label: &str,
    def: &LabelDef,
) -> Result<Vec<ConstraintViolation>, GraphError> {
    if def.exists().is_empty() && def.unique().is_empty() {
        return Ok(Vec::new());
    }
    let mut nodes = NodeSet::new();
    for (key, record) in snapshot.nodes()? {
        if key.label() == label {
            nodes.insert(key.encode()?, (key, record));
        }
    }
    let labels = BTreeMap::from([(label.to_owned(), def.clone())]);
    check_nodes(&labels, &nodes, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(require: &[&str], unique: &[&str]) -> LabelDef {
        LabelDef::new(
            vec!["name".to_owned()],
            BTreeMap::new(),
            require.iter().map(|s| (*s).to_owned()),
            unique.iter().map(|s| (*s).to_owned()),
        )
        .expect("def")
    }

    fn node(name: &str, props: &[(&str, Value)]) -> (Vec<u8>, (NodeKey, NodeRecord)) {
        let key = NodeKey::new("Service", vec![Value::String(name.into())]).expect("key");
        let record = NodeRecord::new(
            std::iter::empty::<String>(),
            props
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect::<BTreeMap<_, _>>(),
        );
        (key.encode().expect("enc"), (key, record))
    }

    fn labels(d: LabelDef) -> BTreeMap<String, LabelDef> {
        BTreeMap::from([("Service".to_owned(), d)])
    }

    #[test]
    fn key_properties_satisfy_existence_by_identity() {
        // `name` is the key: requiring it is vacuously satisfied.
        let nodes: NodeSet = [node("a", &[])].into_iter().collect();
        let violations = check_nodes(&labels(def(&["name"], &[])), &nodes, None).expect("check");
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn missing_required_is_reported_in_key_order() {
        let nodes: NodeSet = [node("b", &[]), node("a", &[])].into_iter().collect();
        let violations = check_nodes(&labels(def(&["tier"], &[])), &nodes, None).expect("check");
        assert_eq!(violations.len(), 2);
        // Sorted by node key regardless of construction order.
        match (&violations[0], &violations[1]) {
            (
                ConstraintViolation::MissingRequired { node: first, .. },
                ConstraintViolation::MissingRequired { node: second, .. },
            ) => {
                assert_eq!(first.key(), &[Value::String("a".into())]);
                assert_eq!(second.key(), &[Value::String("b".into())]);
            }
            other => panic!("expected two MissingRequired, got {other:?}"),
        }
    }

    #[test]
    fn unique_groups_two_or_more() {
        let nodes: NodeSet = [
            node("a", &[("ip", Value::String("x".into()))]),
            node("b", &[("ip", Value::String("x".into()))]),
            node("c", &[("ip", Value::String("y".into()))]),
        ]
        .into_iter()
        .collect();
        let violations = check_nodes(&labels(def(&[], &["ip"])), &nodes, None).expect("check");
        assert_eq!(violations.len(), 1);
        match &violations[0] {
            ConstraintViolation::Unique { nodes, value, .. } => {
                assert_eq!(nodes.len(), 2);
                assert_eq!(value, &Value::String("x".into()));
            }
            other => panic!("expected Unique, got {other:?}"),
        }
    }

    #[test]
    fn focus_filters_to_involved_violations() {
        let (enc_b, _) = node("b", &[]);
        let nodes: NodeSet = [
            // Pre-existing breach, not in focus: not reported.
            node("a", &[]),
            // Focus node with the same breach: reported.
            node("b", &[]),
            // Pre-existing unique pair where only one member is in focus:
            // reported (the focus node participates in the collision).
            node("c", &[("ip", Value::String("x".into()))]),
        ]
        .into_iter()
        .collect();
        let focus: BTreeSet<Vec<u8>> = [enc_b].into_iter().collect();
        let violations =
            check_nodes(&labels(def(&["tier"], &["ip"])), &nodes, Some(&focus)).expect("check");
        assert_eq!(violations.len(), 1);
        assert!(matches!(
            &violations[0],
            ConstraintViolation::MissingRequired { node, .. }
                if node.key() == [Value::String("b".into())]
        ));
    }

    #[test]
    fn absent_unique_property_is_not_a_collision() {
        // UNIQUE constrains values that exist; absence is the existence
        // constraint's business.
        let nodes: NodeSet = [node("a", &[]), node("b", &[])].into_iter().collect();
        let violations = check_nodes(&labels(def(&[], &["ip"])), &nodes, None).expect("check");
        assert!(violations.is_empty(), "{violations:?}");
    }

    #[test]
    fn rendering_is_bounded_and_counts_the_rest() {
        let violations: Vec<ConstraintViolation> = (0..REPORT_LIMIT + 5)
            .map(|i| ConstraintViolation::MissingRequired {
                node: NodeKey::new("Service", vec![Value::Int(i as i64)]).expect("key"),
                property: "tier".to_owned(),
            })
            .collect();
        let rendered = ConstraintViolations(violations).to_string();
        assert!(
            rendered.starts_with("25 constraint violations:"),
            "{rendered}"
        );
        assert!(rendered.contains("… and 5 more"), "{rendered}");
        assert_eq!(rendered.matches("missing required").count(), REPORT_LIMIT);
    }

    #[test]
    fn rendering_escapes_hostile_names() {
        // Labels, properties and values are attacker-writable; the display
        // path must neutralise control characters (the PR #25 bar).
        let v = ConstraintViolation::Unique {
            label: "evil\x1b[8m".into(),
            property: "p\x1b[31m".into(),
            value: Value::String("x\x1b[0m".into()),
            nodes: vec![NodeKey::new("evil\x1b[8m", vec![Value::String("k".into())]).expect("key")],
        };
        assert!(!v.to_string().contains('\x1b'));
    }
}
