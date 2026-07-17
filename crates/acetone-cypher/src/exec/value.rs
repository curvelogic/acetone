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

use acetone_model::Value as ModelValue;

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
    /// A read-only carrier for a stored value the runtime does not model
    /// natively (`Bytes` and the four temporals). The read adapter produces it
    /// so an untouched read→write round-trip recovers the original typed
    /// [`ModelValue`] instead of retyping it to a string (ADR-0038); it is
    /// **never** produced by a Cypher expression. In every query semantic it is
    /// behaviourally identical to [`Value::String`] of its rendering
    /// ([`render_stored`]): `format`, `type_name` and the three comparison
    /// regimes all delegate to that string, and it [`decays`](Value::decayed)
    /// to that string the moment an operator or function consumes it.
    Stored(ModelValue),
}

/// Render a stored value as the string the runtime used before the carrier
/// existed: lowercase hex for `Bytes`, a stable `{:?}` debug rendering for the
/// temporals. This is the frozen string form a [`Value::Stored`] presents in
/// every query semantic (ADR-0038).
pub fn render_stored(mv: &ModelValue) -> String {
    match mv {
        ModelValue::Bytes(bytes) => hex(bytes),
        other => format!("{other:?}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
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
            // A carrier is indistinguishable from its string rendering.
            Value::Stored(_) => "String",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Normalise a read-carrier to its string rendering when an operator or
    /// function is about to consume this value (ADR-0038). Shallow and O(1):
    /// a non-`Stored` value (including a list that merely *contains* a carrier)
    /// is returned untouched, so evaluation never pays a traversal cost and an
    /// untouched pass-through keeps its carrier for the write-back round-trip.
    pub(crate) fn decayed(self) -> Value {
        match self {
            Value::Stored(mv) => Value::String(render_stored(&mv)),
            other => other,
        }
    }

    /// A human-readable rendering for user-facing error messages.
    ///
    /// Mirrors [`acetone_model::display::format_value`]: [`Value::String`] is
    /// escaped with `{:?}` so control characters and ANSI escapes from a
    /// hostile clone are neutralised rather than reaching the terminal raw
    /// (a bare `{other:?}` would leak `String("…")` *and* the raw bytes).
    /// Every variant is handled so a runtime value can never panic the error
    /// path.
    pub fn format(&self) -> String {
        match self {
            Value::Null => "null".to_owned(),
            Value::Bool(b) => b.to_string(),
            Value::Int(n) => n.to_string(),
            Value::Float(x) => x.to_string(),
            Value::String(s) => format!("{s:?}"),
            Value::List(items) => {
                let parts: Vec<String> = items.iter().map(Value::format).collect();
                format!("[{}]", parts.join(", "))
            }
            Value::Map(entries) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(key, value)| format!("{key:?}: {}", value.format()))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Node(node) => {
                let labels: Vec<String> = node
                    .labels
                    .iter()
                    .map(|label| format!("{label:?}"))
                    .collect();
                format!("node({})", labels.join(":"))
            }
            Value::Relationship(rel) => format!("relationship({:?})", rel.rel_type),
            Value::Path(path) => format!("path(length {})", path.rels.len()),
            // Rendered exactly as `Value::String(render_stored(mv))` would be.
            Value::Stored(mv) => format!("{:?}", render_stored(mv)),
        }
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
            // A carrier compares as its string rendering (ADR-0038); recursion
            // reaches a carrier nested inside a list or map. Terminates: the
            // rendering is a `String`, never another `Stored`.
            (Stored(mv), _) => String(render_stored(mv)).eq3(other),
            (_, Stored(mv)) => self.eq3(&String(render_stored(mv))),
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
            // A carrier orders as its string rendering (ADR-0038).
            (Stored(mv), _) => String(render_stored(mv)).cmp3(other),
            (_, Stored(mv)) => self.cmp3(&String(render_stored(mv))),
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
                // Unreachable: a carrier is normalised to its `String` rendering
                // at the top of `global_cmp` before `rank` is ever called. Ranked
                // as a string for a coherent total order regardless.
                Value::Stored(_) => 5,
            }
        }
        use Value::*;
        match (self, other) {
            // A carrier sorts as its string rendering (ADR-0038): it ranks
            // among strings, and two carriers order by their renderings.
            (Stored(mv), _) => String(render_stored(mv)).global_cmp(other),
            (_, Stored(mv)) => self.global_cmp(&String(render_stored(mv))),
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

    /// A canonical, self-delimiting byte key for hash-based DISTINCT/grouping
    /// dedup. Two values that are [`equivalent`](Value::equivalent) produce the
    /// **same** key, so a `HashSet<Vec<u8>>` dedups in O(n) instead of the
    /// O(n²) linear-scan `equivalent` comparison — the difference between the
    /// governor bounding a `DISTINCT` and a query running away with the CPU
    /// (acetone-8ln). Every variable-length field is length-prefixed, so keys
    /// never collide across shapes.
    ///
    /// The one place key-equality is *finer* than `equivalent`: two numbers
    /// whose `f64` images coincide only because a large integer is not exactly
    /// representable (e.g. `2^60` vs `2^60 + 1`) get distinct keys, where
    /// `equivalent` — comparing through `f64` — calls them equal. `equivalent`
    /// is not transitive there (it is a lossy numeric comparison, not a true
    /// equivalence relation), so no hash key can match it in that corner;
    /// treating genuinely-distinct integers as distinct is the safer reading
    /// and never affects a realistic `DISTINCT`.
    pub fn distinct_key(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.push_distinct_key(&mut out);
        out
    }

    fn push_distinct_key(&self, out: &mut Vec<u8>) {
        fn len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            out.extend_from_slice(bytes);
        }
        // Tags match the `rank` used by `global_cmp` so the key's type ordering
        // is consistent with the sort order; a carrier is keyed as its string.
        match self {
            Value::Stored(mv) => Value::String(render_stored(mv)).push_distinct_key(out),
            Value::Map(m) => {
                out.push(0);
                out.extend_from_slice(&(m.len() as u64).to_be_bytes());
                for (k, v) in m {
                    len_prefixed(out, k.as_bytes());
                    v.push_distinct_key(out);
                }
            }
            Value::Node(n) => {
                out.push(1);
                len_prefixed(out, &n.id.0);
            }
            Value::Relationship(r) => {
                out.push(2);
                len_prefixed(out, &r.id.0);
            }
            Value::List(items) => {
                out.push(3);
                out.extend_from_slice(&(items.len() as u64).to_be_bytes());
                for item in items {
                    item.push_distinct_key(out);
                }
            }
            Value::Path(p) => {
                out.push(4);
                out.extend_from_slice(&((p.nodes.len() + p.rels.len()) as u64).to_be_bytes());
                for n in &p.nodes {
                    len_prefixed(out, &n.id.0);
                }
                for r in &p.rels {
                    len_prefixed(out, &r.id.0);
                }
            }
            Value::String(s) => {
                out.push(5);
                len_prefixed(out, s.as_bytes());
            }
            Value::Bool(b) => {
                out.push(6);
                out.push(*b as u8);
            }
            Value::Int(_) | Value::Float(_) => {
                out.push(7);
                push_number_key(self, out);
            }
            Value::Null => out.push(8),
        }
    }
}

/// Key the numeric sub-domain the way `global_cmp` compares numbers: `NaN` is
/// one bucket; a value equal to an exact `i64` (an `Int`, or an integral
/// `Float` in range) keys by that `i64` so `Int(1) ≡ Float(1.0)`; any other
/// finite/infinite float keys by its bits. See [`Value::distinct_key`] for the
/// one lossy-comparison corner this does not (and cannot) reproduce.
fn push_number_key(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(n) => {
            out.push(0);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Float(f) if f.is_nan() => out.push(2),
        Value::Float(f) if f.fract() == 0.0 && *f >= -(2f64.powi(63)) && *f < 2f64.powi(63) => {
            out.push(0);
            out.extend_from_slice(&(*f as i64).to_be_bytes());
        }
        Value::Float(f) => {
            out.push(1);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        _ => unreachable!("push_number_key called on a non-number"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> Value {
        Value::Int(n)
    }

    #[test]
    fn render_stored_pins_the_frozen_string_for_every_deferred_type() {
        // The carrier's string rendering is a frozen contract: it IS the runtime
        // string a Bytes/temporal property presents in every query semantic, and
        // it is the key an index buckets that property under. An accidental change
        // to a `Debug` derive on `acetone_model::Value` (or a temporal struct)
        // would silently shift both — pin the exact bytes here so it lands red.
        use acetone_model::{Date, DateTime, Duration, Time};
        assert_eq!(
            render_stored(&ModelValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef])),
            "deadbeef"
        );
        assert_eq!(
            render_stored(&ModelValue::Date(Date { days: 20_000 })),
            "Date(Date { days: 20000 })"
        );
        assert_eq!(
            render_stored(&ModelValue::Time(Time {
                nanos: 3_600_000_000_000
            })),
            "Time(Time { nanos: 3600000000000 })"
        );
        assert_eq!(
            render_stored(&ModelValue::DateTime(DateTime {
                epoch_nanos: 1_600_000_000_000_000_000,
                offset_minutes: 60,
            })),
            "DateTime(DateTime { epoch_nanos: 1600000000000000000, offset_minutes: 60 })"
        );
        assert_eq!(
            render_stored(&ModelValue::Duration(Duration {
                months: 1,
                days: 2,
                nanos: 3,
            })),
            "Duration(Duration { months: 1, days: 2, nanos: 3 })"
        );
        // Empty Bytes renders to the empty string (no `0x` prefix, no padding).
        assert_eq!(render_stored(&ModelValue::Bytes(vec![])), "");
    }

    #[test]
    fn a_carrier_behaves_as_its_string_rendering() {
        let carrier = Value::Stored(ModelValue::Bytes(vec![0xde, 0xad]));
        let string = Value::String("dead".into());
        // type_name, format and all three comparison regimes agree with the string.
        assert_eq!(carrier.type_name(), "String");
        assert_eq!(carrier.format(), string.format());
        assert_eq!(carrier.eq3(&string), Some(true));
        assert_eq!(carrier.cmp3(&string), Some(Ordering::Equal));
        assert_eq!(carrier.global_cmp(&string), Ordering::Equal);
        // A carrier nested in a list compares element-wise via the same delegation.
        let cl = Value::List(vec![Value::Stored(ModelValue::Bytes(vec![0xde, 0xad]))]);
        let sl = Value::List(vec![Value::String("dead".into())]);
        assert_eq!(cl.eq3(&sl), Some(true));
        assert!(cl.equivalent(&sl));
    }

    #[test]
    fn distinct_key_agrees_with_equivalent() {
        use acetone_model::Value as MV;
        // Representatives spanning every rank and the tricky within-rank cases.
        let values = vec![
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Int(0),
            Value::Int(1),
            Value::Int(-1),
            Value::Float(1.0), // ≡ Int(1)
            Value::Float(1.5),
            Value::Float(f64::NAN),
            Value::Float(f64::INFINITY),
            Value::String(String::new()),
            Value::String("a".into()),
            Value::String("ab".into()),
            Value::Stored(MV::Bytes(vec![0xab])), // ≡ String("ab")
            Value::List(vec![Value::Int(1), Value::String("a".into())]),
            Value::List(vec![Value::Int(1)]),
            Value::Map(BTreeMap::from([("k".to_string(), Value::Int(1))])),
        ];
        // The key partitions values exactly as `equivalent` does (over this set,
        // which avoids the documented lossy-numeric corner): equal keys iff
        // equivalent, and — critically — NaN≡NaN, Int(1)≡Float(1.0),
        // Stored(0xab)≡String("ab").
        for a in &values {
            for b in &values {
                assert_eq!(
                    a.distinct_key() == b.distinct_key(),
                    a.equivalent(b),
                    "distinct_key/equivalent disagree on {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn distinct_key_unions_the_number_domain() {
        // Int and an integral Float share a key (they are equivalent)…
        assert_eq!(
            Value::Int(42).distinct_key(),
            Value::Float(42.0).distinct_key()
        );
        // …NaNs collapse to one bucket (NaN ≡ NaN for DISTINCT)…
        assert_eq!(
            Value::Float(f64::NAN).distinct_key(),
            Value::Float(-f64::NAN).distinct_key()
        );
        // …but a non-integral float is its own value.
        assert_ne!(
            Value::Int(42).distinct_key(),
            Value::Float(42.5).distinct_key()
        );
    }

    #[test]
    fn distinct_key_is_collision_free_across_tuple_boundaries() {
        // Self-delimiting keys: the tuple ("a","bc") must not key-collide with
        // ("ab","c"), which a naive concatenation would.
        let concat = |vs: &[Value]| -> Vec<u8> {
            let mut k = Vec::new();
            for v in vs {
                k.extend(v.distinct_key());
            }
            k
        };
        let ab_c = concat(&[Value::String("ab".into()), Value::String("c".into())]);
        let a_bc = concat(&[Value::String("a".into()), Value::String("bc".into())]);
        assert_ne!(ab_c, a_bc);
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
    fn format_escapes_strings_and_renders_scalars() {
        assert_eq!(int(42).format(), "42");
        assert_eq!(Value::Bool(true).format(), "true");
        assert_eq!(Value::Null.format(), "null");
        // A plain string is escaped (quoted), never leaking `String(…)`.
        assert_eq!(Value::String("x".into()).format(), "\"x\"");
        assert_eq!(
            Value::List(vec![int(1), Value::String("a".into())]).format(),
            "[1, \"a\"]"
        );
    }

    #[test]
    fn format_neutralises_control_characters() {
        // A hostile string bound (e.g. `SKIP '…'`) must never reach the
        // terminal raw — the whole reason this formatter exists.
        let rendered = Value::String("a\x1b[31mb\nc".into()).format();
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\n'));
        assert_eq!(rendered, "\"a\\u{1b}[31mb\\nc\"");
    }

    #[test]
    fn equivalence_unifies_null_and_nan() {
        assert!(Value::Null.equivalent(&Value::Null));
        assert!(Value::Float(f64::NAN).equivalent(&Value::Float(f64::NAN)));
        assert!(int(1).equivalent(&Value::Float(1.0)));
        assert!(!int(1).equivalent(&Value::String("1".into())));
    }
}
