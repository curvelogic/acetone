//! The runtime value model and openCypher's three comparison regimes,
//! which the TCK distinguishes precisely:
//!
//! - **Equality** (`=`, ternary): null propagates; numbers compare across
//!   Int/Float; cross-type equality is false; lists/maps compare
//!   element-wise ternarily.
//! - **Ordering comparison** (`<` etc., ternary): only mutually
//!   comparable values order; cross-type and null yield null; NaN
//!   compares false against everything including itself.
//! - **Global sort order** (ORDER BY/min/max, total): maps < nodes <
//!   relationships < lists < paths < strings < booleans < numbers < null,
//!   NaN after all other numbers; used for DISTINCT/grouping equivalence
//!   too, where null equals itself and NaN equals itself.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Opaque, stable entity identity (in-memory counter bytes or storage key
/// bytes). Equality of entities is identity, not property equality.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(pub Arc<[u8]>);

impl EntityId {
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        EntityId(bytes.into().into())
    }
}

#[derive(Debug, Clone)]
pub struct NodeValue {
    pub id: EntityId,
    pub labels: Vec<String>,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct RelValue {
    pub id: EntityId,
    pub rel_type: String,
    pub start: EntityId,
    pub end: EntityId,
    pub properties: BTreeMap<String, Value>,
}

/// A path: alternating nodes and relationships, starting and ending with
/// a node.
#[derive(Debug, Clone)]
pub struct PathValue {
    pub nodes: Vec<NodeValue>,
    pub rels: Vec<RelValue>,
}

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Node(NodeValue),
    Relationship(RelValue),
    Path(PathValue),
}

/// Ternary logic result.
pub type Ternary = Option<bool>;

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "Null",
            Value::Bool(_) => "Boolean",
            Value::Int(_) => "Integer",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::List(_) => "List",
            Value::Map(_) => "Map",
            Value::Node(_) => "Node",
            Value::Relationship(_) => "Relationship",
            Value::Path(_) => "Path",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(x) => Some(*x),
            _ => None,
        }
    }

    /// openCypher equality (`=`): ternary.
    pub fn eq3(&self, other: &Value) -> Ternary {
        use Value::*;
        match (self, other) {
            (Null, _) | (_, Null) => None,
            (Bool(a), Bool(b)) => Some(a == b),
            (Int(a), Int(b)) => Some(a == b),
            (Int(_), Float(_)) | (Float(_), Int(_)) | (Float(_), Float(_)) => {
                let (a, b) = (self.as_f64().unwrap(), other.as_f64().unwrap());
                Some(a == b) // NaN = anything is false
            }
            (String(a), String(b)) => Some(a == b),
            (List(a), List(b)) => {
                if a.len() != b.len() {
                    return Some(false);
                }
                let mut saw_null = false;
                for (x, y) in a.iter().zip(b) {
                    match x.eq3(y) {
                        Some(false) => return Some(false),
                        None => saw_null = true,
                        Some(true) => {}
                    }
                }
                if saw_null { None } else { Some(true) }
            }
            (Map(a), Map(b)) => {
                if a.len() != b.len() || !a.keys().eq(b.keys()) {
                    return Some(false);
                }
                let mut saw_null = false;
                for (key, x) in a {
                    match x.eq3(&b[key]) {
                        Some(false) => return Some(false),
                        None => saw_null = true,
                        Some(true) => {}
                    }
                }
                if saw_null { None } else { Some(true) }
            }
            (Node(a), Node(b)) => Some(a.id == b.id),
            (Relationship(a), Relationship(b)) => Some(a.id == b.id),
            (Path(a), Path(b)) => Some(
                a.nodes.len() == b.nodes.len()
                    && a.nodes.iter().zip(&b.nodes).all(|(x, y)| x.id == y.id)
                    && a.rels.iter().zip(&b.rels).all(|(x, y)| x.id == y.id),
            ),
            // Cross-type equality is false, not null.
            _ => Some(false),
        }
    }

    /// openCypher ordering comparison (`<` and friends): ternary. Only
    /// mutually comparable values order: numbers with numbers, strings
    /// with strings, booleans with booleans, lists with lists.
    pub fn cmp3(&self, other: &Value) -> Option<Ordering> {
        use Value::*;
        match (self, other) {
            (Null, _) | (_, Null) => None,
            (Int(a), Int(b)) => Some(a.cmp(b)),
            (Int(_), Float(_)) | (Float(_), Int(_)) | (Float(_), Float(_)) => {
                let (a, b) = (self.as_f64().unwrap(), other.as_f64().unwrap());
                a.partial_cmp(&b) // NaN: incomparable -> None
            }
            (String(a), String(b)) => Some(a.cmp(b)),
            (Bool(a), Bool(b)) => Some(a.cmp(b)),
            (List(a), List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    match x.cmp3(y)? {
                        Ordering::Equal => continue,
                        unequal => return Some(unequal),
                    }
                }
                Some(a.len().cmp(&b.len()))
            }
            _ => None,
        }
    }

    /// The global sort order (ORDER BY): total over all values.
    /// openCypher CIP: Map < Node < Relationship < List < Path < String <
    /// Boolean < Number; null greatest; NaN after all other numbers.
    pub fn global_cmp(&self, other: &Value) -> Ordering {
        fn rank(value: &Value) -> u8 {
            match value {
                Value::Map(_) => 0,
                Value::Node(_) => 1,
                Value::Relationship(_) => 2,
                Value::List(_) => 3,
                Value::Path(_) => 4,
                Value::String(_) => 5,
                Value::Bool(_) => 6,
                Value::Int(_) | Value::Float(_) => 7,
                Value::Null => 8,
            }
        }
        use Value::*;
        match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Int(a), Int(b)) => a.cmp(b),
            (Int(_), Float(_)) | (Float(_), Int(_)) | (Float(_), Float(_)) => {
                let (a, b) = (self.as_f64().unwrap(), other.as_f64().unwrap());
                match (a.is_nan(), b.is_nan()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater, // NaN after numbers
                    (false, true) => Ordering::Less,
                    (false, false) => a.partial_cmp(&b).expect("not NaN"),
                }
            }
            (String(a), String(b)) => a.cmp(b),
            // false < true.
            (Bool(a), Bool(b)) => a.cmp(b),
            (List(a), List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    match x.global_cmp(y) {
                        Ordering::Equal => continue,
                        unequal => return unequal,
                    }
                }
                a.len().cmp(&b.len())
            }
            (Map(a), Map(b)) => {
                // Deterministic: by sorted key sequence then values.
                let keys = a.keys().cmp(b.keys());
                if keys != Ordering::Equal {
                    return keys;
                }
                for (key, x) in a {
                    match x.global_cmp(&b[key]) {
                        Ordering::Equal => continue,
                        unequal => return unequal,
                    }
                }
                Ordering::Equal
            }
            (Node(a), Node(b)) => a.id.cmp(&b.id),
            (Relationship(a), Relationship(b)) => a.id.cmp(&b.id),
            (Path(a), Path(b)) => {
                let ids = |p: &PathValue| -> Vec<EntityId> {
                    p.nodes
                        .iter()
                        .map(|n| n.id.clone())
                        .chain(p.rels.iter().map(|r| r.id.clone()))
                        .collect()
                };
                ids(a).cmp(&ids(b))
            }
            _ => rank(self).cmp(&rank(other)),
        }
    }

    /// Equivalence for DISTINCT and grouping: global order equality
    /// (null ≡ null, NaN ≡ NaN).
    pub fn equivalent(&self, other: &Value) -> bool {
        self.global_cmp(other) == Ordering::Equal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn equality_is_ternary() {
        assert_eq!(Value::Null.eq3(&Value::Null), None);
        assert_eq!(int(1).eq3(&Value::Null), None);
        assert_eq!(int(1).eq3(&int(1)), Some(true));
        assert_eq!(int(1).eq3(&Value::Float(1.0)), Some(true));
        // Cross-type equality is false, not null.
        assert_eq!(int(1).eq3(&Value::String("1".into())), Some(false));
        assert_eq!(Value::Bool(true).eq3(&int(1)), Some(false));
    }

    #[test]
    fn list_equality_propagates_null_elementwise() {
        let a = Value::List(vec![int(1), Value::Null]);
        let b = Value::List(vec![int(1), Value::Null]);
        assert_eq!(a.eq3(&b), None);
        let c = Value::List(vec![int(2), Value::Null]);
        assert_eq!(a.eq3(&c), Some(false));
        let short = Value::List(vec![int(1)]);
        assert_eq!(a.eq3(&short), Some(false));
    }

    #[test]
    fn nan_equality_and_comparison() {
        let nan = Value::Float(f64::NAN);
        assert_eq!(nan.eq3(&nan), Some(false));
        assert_eq!(nan.eq3(&Value::Float(1.0)), Some(false));
        assert_eq!(nan.cmp3(&Value::Float(1.0)), None);
    }

    #[test]
    fn cross_type_comparison_is_null() {
        assert_eq!(int(1).cmp3(&Value::String("a".into())), None);
        assert_eq!(Value::Bool(true).cmp3(&int(1)), None);
        assert_eq!(int(1).cmp3(&Value::Null), None);
    }

    #[test]
    fn global_order_ranks_types_and_places_null_last() {
        let mut values = [
            Value::Null,
            int(5),
            Value::Float(f64::NAN),
            Value::String("a".into()),
            Value::Bool(false),
            Value::List(vec![int(1)]),
            Value::Map(BTreeMap::new()),
        ];
        values.sort_by(|a, b| a.global_cmp(b));
        let names: Vec<&str> = values.iter().map(|v| v.type_name()).collect();
        assert_eq!(
            names,
            vec![
                "Map", "List", "String", "Boolean", "Integer", "Float", "Null"
            ]
        );
        // NaN sorts after ordinary numbers, before null.
        assert!(matches!(values[5], Value::Float(x) if x.is_nan()));
    }

    #[test]
    fn equivalence_unifies_null_and_nan() {
        assert!(Value::Null.equivalent(&Value::Null));
        assert!(Value::Float(f64::NAN).equivalent(&Value::Float(f64::NAN)));
        assert!(int(1).equivalent(&Value::Float(1.0)));
        assert!(!int(1).equivalent(&Value::String("1".into())));
    }
}
