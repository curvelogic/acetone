//! Cell-wise (per-property) three-way merge of node and edge records
//! (ADR-0035, `acetone-clm`).
//!
//! The prolly three-way merge treats each map value opaquely, so a node or edge
//! modified on both branches is a whole-record conflict — even when the two
//! branches edited *different* properties. This module refines that: when the
//! map merge flags a key modified on both sides, the graph layer decodes the
//! base/ours/theirs records and merges their **contents**:
//!
//! - **Properties** — per-property three-way: a one-sided change is taken; both
//!   sides to the *same* value merge clean; both sides to *different* values,
//!   and add-vs-modify / delete-vs-modify, are per-property conflicts.
//! - **Secondary labels** — set-wise (a label added or removed on either side,
//!   relative to base, applies; the two deltas never conflict).
//! - **Key properties** are never here — they live in the map key, so node
//!   identity is untouched (Load-Bearing Invariant #3).
//!
//! ## Determinism (Load-Bearing Invariant #4)
//!
//! The merge is a pure function over the **sorted** property key set (a
//! `BTreeMap`) and the sorted label set, so it is iteration-order independent.
//! Value equality is canonical-CBOR equality ([`encode_value`]), which
//! canonicalises `NaN` — so `NaN` on all three sides merges clean rather than
//! spuriously conflicting.

use std::collections::BTreeMap;

use acetone_model::Value;
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::values::encode_value;

use crate::GraphError;

/// A per-property clash within a record modified on both branches: the property
/// diverged (different values, or add-vs-modify, or delete-vs-modify). The
/// property is excluded from the merged record until resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyConflict {
    /// The conflicted property name.
    pub property: String,
    /// The value in the merge base (`None` if the property was absent there).
    pub base: Option<Value>,
    /// The value on our side (`None` if we deleted or never had it).
    pub ours: Option<Value>,
    /// The value on their side (`None` if they deleted or never had it).
    pub theirs: Option<Value>,
}

/// Canonical value equality — the same NaN-canonicalising deterministic CBOR the
/// storage layer compares by, so the merge agrees with history independence.
fn value_eq(a: &Value, b: &Value) -> Result<bool, GraphError> {
    Ok(encode_value(a)? == encode_value(b)?)
}

/// Three-way equality of an optional property value; two absent values are equal.
fn opt_eq(a: Option<&Value>, b: Option<&Value>) -> Result<bool, GraphError> {
    match (a, b) {
        (None, None) => Ok(true),
        (Some(x), Some(y)) => value_eq(x, y),
        _ => Ok(false),
    }
}

/// Per-property three-way merge of two property maps against their base. Returns
/// the auto-merged properties and any per-property conflicts (in sorted-key
/// order, so deterministic).
pub fn merge_properties(
    base: &BTreeMap<String, Value>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
) -> Result<(BTreeMap<String, Value>, Vec<PropertyConflict>), GraphError> {
    // Union of property names, sorted (BTreeSet) for deterministic iteration.
    let mut names: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
    names.extend(base.keys());
    names.extend(ours.keys());
    names.extend(theirs.keys());

    let mut merged = BTreeMap::new();
    let mut conflicts = Vec::new();
    for name in names {
        let b = base.get(name);
        let o = ours.get(name);
        let t = theirs.get(name);
        // Resolve to the value the merged record should hold (or `None` to omit
        // a deleted property), else record a conflict.
        let resolved: Option<Option<&Value>> = if opt_eq(o, t)? {
            // Both sides agree (same value, or both deleted).
            Some(o)
        } else if opt_eq(o, b)? {
            // Ours is unchanged from base → take theirs' change.
            Some(t)
        } else if opt_eq(t, b)? {
            // Theirs is unchanged from base → take ours' change.
            Some(o)
        } else {
            // Both sides changed the property differently (incl. add-vs-modify
            // and delete-vs-modify): a per-property conflict.
            conflicts.push(PropertyConflict {
                property: name.clone(),
                base: b.cloned(),
                ours: o.cloned(),
                theirs: t.cloned(),
            });
            None
        };
        if let Some(Some(value)) = resolved {
            merged.insert(name.clone(), value.clone());
        }
    }
    Ok((merged, conflicts))
}

/// Set-wise three-way merge of secondary labels: a label added or removed on
/// either side relative to base applies. The two deltas cannot conflict — for
/// any label, if the two sides disagree then exactly one of them equals the
/// base state, so that side's change is the delta and the other is base.
/// Returns a sorted, deduped label list (matching `NodeRecord::new`).
pub fn merge_labels(base: &[String], ours: &[String], theirs: &[String]) -> Vec<String> {
    use std::collections::BTreeSet;
    let base: BTreeSet<&String> = base.iter().collect();
    let ours: BTreeSet<&String> = ours.iter().collect();
    let theirs: BTreeSet<&String> = theirs.iter().collect();

    let mut all: BTreeSet<&String> = BTreeSet::new();
    all.extend(base.iter().copied());
    all.extend(ours.iter().copied());
    all.extend(theirs.iter().copied());

    all.into_iter()
        .filter(|label| {
            let in_base = base.contains(label);
            let removed =
                (in_base && !ours.contains(label)) || (in_base && !theirs.contains(label));
            let present = in_base || ours.contains(label) || theirs.contains(label);
            present && !removed
        })
        .cloned()
        .collect()
}

/// Cell-wise merge of a node record modified on both branches: labels set-wise,
/// properties per-property. Conflicted properties are omitted from the merged
/// record (surfaced separately) so the auto-merged ones still land.
pub fn merge_node_record(
    base: &NodeRecord,
    ours: &NodeRecord,
    theirs: &NodeRecord,
) -> Result<(NodeRecord, Vec<PropertyConflict>), GraphError> {
    let labels = merge_labels(
        base.secondary_labels(),
        ours.secondary_labels(),
        theirs.secondary_labels(),
    );
    let (properties, conflicts) =
        merge_properties(base.properties(), ours.properties(), theirs.properties())?;
    Ok((NodeRecord::new(labels, properties), conflicts))
}

/// Cell-wise merge of an edge record (properties only — edges carry no labels).
pub fn merge_edge_record(
    base: &EdgeRecord,
    ours: &EdgeRecord,
    theirs: &EdgeRecord,
) -> Result<(EdgeRecord, Vec<PropertyConflict>), GraphError> {
    let (properties, conflicts) =
        merge_properties(base.properties(), ours.properties(), theirs.properties())?;
    Ok((EdgeRecord::new(properties), conflicts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn divergent_properties_auto_merge() {
        // The flagship case: import sets os_version, a human sets owner, on the
        // same node — different properties, so no conflict.
        let base = props(&[("name", Value::String("web".into()))]);
        let ours = props(&[
            ("name", Value::String("web".into())),
            ("owner", Value::String("greg".into())),
        ]);
        let theirs = props(&[
            ("name", Value::String("web".into())),
            ("os_version", Value::String("12".into())),
        ]);
        let (merged, conflicts) = merge_properties(&base, &ours, &theirs).unwrap();
        assert!(
            conflicts.is_empty(),
            "divergent properties must not conflict"
        );
        assert_eq!(merged.get("owner"), Some(&Value::String("greg".into())));
        assert_eq!(merged.get("os_version"), Some(&Value::String("12".into())));
        assert_eq!(merged.get("name"), Some(&Value::String("web".into())));
    }

    #[test]
    fn same_property_different_values_conflicts() {
        let base = props(&[("v", int(0))]);
        let ours = props(&[("v", int(1))]);
        let theirs = props(&[("v", int(2))]);
        let (merged, conflicts) = merge_properties(&base, &ours, &theirs).unwrap();
        assert!(
            !merged.contains_key("v"),
            "a conflicted property is omitted"
        );
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].property, "v");
        assert_eq!(conflicts[0].base, Some(int(0)));
        assert_eq!(conflicts[0].ours, Some(int(1)));
        assert_eq!(conflicts[0].theirs, Some(int(2)));
    }

    #[test]
    fn same_property_same_value_is_clean() {
        let base = props(&[("v", int(0))]);
        let ours = props(&[("v", int(5))]);
        let theirs = props(&[("v", int(5))]);
        let (merged, conflicts) = merge_properties(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(merged.get("v"), Some(&int(5)));
    }

    #[test]
    fn one_sided_change_is_taken() {
        let base = props(&[("v", int(0))]);
        let ours = props(&[("v", int(9))]); // ours changed
        let theirs = props(&[("v", int(0))]); // theirs unchanged
        let (merged, conflicts) = merge_properties(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(merged.get("v"), Some(&int(9)));
    }

    #[test]
    fn add_vs_modify_conflicts() {
        // base absent; ours adds one value, theirs... base has it, theirs
        // modifies, ours absent → delete-vs-modify. Here: base absent, ours adds
        // 1, theirs adds 2 → add-vs-add different = conflict.
        let base = props(&[]);
        let ours = props(&[("v", int(1))]);
        let theirs = props(&[("v", int(2))]);
        let (_merged, conflicts) = merge_properties(&base, &ours, &theirs).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].base, None);
    }

    #[test]
    fn delete_vs_modify_conflicts() {
        let base = props(&[("v", int(0))]);
        let ours = props(&[]); // ours deleted v
        let theirs = props(&[("v", int(7))]); // theirs modified v
        let (merged, conflicts) = merge_properties(&base, &ours, &theirs).unwrap();
        assert!(!merged.contains_key("v"));
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].ours, None);
        assert_eq!(conflicts[0].theirs, Some(int(7)));
    }

    #[test]
    fn both_delete_is_clean() {
        let base = props(&[("v", int(0))]);
        let ours = props(&[]);
        let theirs = props(&[]);
        let (merged, conflicts) = merge_properties(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty());
        assert!(!merged.contains_key("v"));
    }

    #[test]
    fn nan_on_all_sides_is_clean_not_a_conflict() {
        // Canonical CBOR equality treats NaN == NaN, unlike PartialEq — so a
        // property that is NaN everywhere merges clean and deterministically.
        let base = props(&[("v", Value::Float(f64::NAN))]);
        let ours = props(&[("v", Value::Float(f64::NAN)), ("x", int(1))]);
        let theirs = props(&[("v", Value::Float(f64::NAN)), ("y", int(2))]);
        let (merged, conflicts) = merge_properties(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty(), "NaN on all sides must not conflict");
        assert!(matches!(merged.get("v"), Some(Value::Float(f)) if f.is_nan()));
        assert_eq!(merged.get("x"), Some(&int(1)));
        assert_eq!(merged.get("y"), Some(&int(2)));
    }

    #[test]
    fn merge_is_symmetric_up_to_conflict_sides() {
        // Swapping ours/theirs yields the same merged content and mirror-image
        // conflict sides — the determinism/symmetry property (Invariant #4).
        let base = props(&[("v", int(0)), ("a", int(1))]);
        let ours = props(&[("v", int(1)), ("a", int(1)), ("o", int(9))]);
        let theirs = props(&[("v", int(2)), ("a", int(1)), ("t", int(8))]);
        let (m1, c1) = merge_properties(&base, &ours, &theirs).unwrap();
        let (m2, c2) = merge_properties(&base, &theirs, &ours).unwrap();
        assert_eq!(m1, m2, "merged content is order-independent");
        assert_eq!(c1.len(), 1);
        assert_eq!(c2.len(), 1);
        // Conflict sides mirror.
        assert_eq!(c1[0].ours, c2[0].theirs);
        assert_eq!(c1[0].theirs, c2[0].ours);
    }

    #[test]
    fn labels_merge_set_wise() {
        // base {A}; ours adds B; theirs removes A → merged {B}.
        let merged = merge_labels(
            &["A".into()],
            &["A".into(), "B".into()],
            &[], // theirs removed A
        );
        assert_eq!(merged, vec!["B".to_string()]);

        // both add the same label → present once.
        let merged = merge_labels(&[], &["X".into()], &["X".into()]);
        assert_eq!(merged, vec!["X".to_string()]);

        // one adds, one keeps base → union.
        let merged = merge_labels(&["A".into()], &["A".into(), "B".into()], &["A".into()]);
        assert_eq!(merged, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn node_record_merges_labels_and_properties_together() {
        let base = NodeRecord::new(vec!["A".into()], props(&[("v", int(0))]));
        let ours = NodeRecord::new(
            vec!["A".into(), "B".into()],
            props(&[("v", int(0)), ("owner", Value::String("g".into()))]),
        );
        let theirs = NodeRecord::new(
            vec!["A".into()],
            props(&[("v", int(0)), ("os", Value::String("12".into()))]),
        );
        let (merged, conflicts) = merge_node_record(&base, &ours, &theirs).unwrap();
        assert!(conflicts.is_empty());
        assert_eq!(
            merged.secondary_labels(),
            &["A".to_string(), "B".to_string()]
        );
        assert_eq!(
            merged.properties().get("owner"),
            Some(&Value::String("g".into()))
        );
        assert_eq!(
            merged.properties().get("os"),
            Some(&Value::String("12".into()))
        );
    }
}
