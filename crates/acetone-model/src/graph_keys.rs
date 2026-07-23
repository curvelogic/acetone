//! Graph-level key layouts for the v0.1 maps (spec §3.3, ADR-0008).
//!
//! Composes the memcomparable primitives of [`crate::keys`] into the map
//! keys of `nodes`, `edges_fwd`, `edges_rev` and `idx/<name>`:
//!
//! - a **node key** ([`NodeKey`], Load-Bearing Invariant 3) has exactly
//!   one encoded form everywhere in the format — a single `List` element
//!   `List([String(primary_label), key_0 … key_n])` — used verbatim as
//!   the `nodes`-map key and embedded inside edge and index keys. The
//!   list wrapper makes the variable-arity key tuple self-delimiting in
//!   composite keys; the list terminator sorts below every type tag, so
//!   prefix scans group correctly;
//! - an **edge key** ([`EdgeKey`]) encodes forward as
//!   `[node_key(src), String(type), node_key(dst), disc]` and reverse as
//!   `[node_key(dst), String(type), node_key(src), disc]`; the
//!   discriminator is `Null` unless the relationship type declares one
//!   (spec §2);
//! - an **index entry** ([`IndexEntry`]) encodes as
//!   `[String(label), List(property names), List(values), node_key]` with an
//!   empty map value (spec §3.3); a composite index keys on the ordered value
//!   tuple, a single-property index on a one-element tuple (ADR-0027).
//!
//! Prefix helpers return the byte prefixes that make the spec's scan
//! shapes ("all nodes with label L", "all edges out of X", "all T-edges
//! out of X", "index probes") plain prolly range scans; pair them with
//! [`prefix_successor`] to form half-open ranges.
//!
//! Any change to these layouts is a `format_version` bump (spec §10).

use crate::Value;
use crate::keys::{self, KeyDecodeError, KeyEncodeError};
use thiserror::Error;

/// Errors from constructing or decoding graph keys.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum GraphKeyError {
    /// A label, relationship type or property name was empty.
    #[error("empty name in key position: {0}")]
    EmptyName(&'static str),
    /// A key tuple had no elements (spec §2: non-empty).
    #[error("node key tuple must be non-empty")]
    EmptyKeyTuple,
    /// A key tuple element was null (spec §2: non-null).
    #[error("null is not permitted in a node key tuple")]
    NullKeyValue,
    /// A key tuple element or discriminator was a list (spec §2: scalar).
    #[error("lists are not permitted in {0}")]
    NonScalar(&'static str),
    /// A NaN float in a key position (ADR-0004).
    #[error("NaN is not permitted in {0}")]
    NanKeyValue(&'static str),
    /// The underlying key encoding rejected a value (temporal range).
    #[error(transparent)]
    Encode(#[from] KeyEncodeError),
    /// The underlying key encoding rejected the bytes.
    #[error(transparent)]
    Decode(#[from] KeyDecodeError),
    /// Well-formed key bytes whose tuple shape is not the expected layout.
    #[error("unexpected key shape: {0}")]
    Shape(&'static str),
}

fn check_scalar(v: &Value, context: &'static str) -> Result<(), GraphKeyError> {
    match v {
        Value::List(_) => Err(GraphKeyError::NonScalar(context)),
        Value::Float(x) if x.is_nan() => Err(GraphKeyError::NanKeyValue(context)),
        _ => Ok(()),
    }
}

/// A node's identity: `(primary label, key tuple)` (spec §2, Invariant 3).
///
/// Construction validates the spec's identity rules — non-empty label,
/// non-empty tuple of non-null scalars, no NaN — so a `NodeKey` in
/// existence is always encodable (bar temporal range errors surfaced at
/// encode time). Equality is structural; `-0.0` equals `0.0`, matching
/// the encoded form (ADR-0004 normalisation).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeKey {
    label: String,
    key: Vec<Value>,
}

impl NodeKey {
    /// Validate and construct a node key.
    pub fn new(label: impl Into<String>, key: Vec<Value>) -> Result<Self, GraphKeyError> {
        let label = label.into();
        if label.is_empty() {
            return Err(GraphKeyError::EmptyName("primary label"));
        }
        if key.is_empty() {
            return Err(GraphKeyError::EmptyKeyTuple);
        }
        for v in &key {
            if matches!(v, Value::Null) {
                return Err(GraphKeyError::NullKeyValue);
            }
            check_scalar(v, "node key tuple")?;
        }
        Ok(NodeKey { label, key })
    }

    /// The primary label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The key tuple values.
    pub fn key(&self) -> &[Value] {
        &self.key
    }

    /// The node key as its single-element logical form:
    /// `List([String(label), key_0 … key_n])`.
    pub fn to_value(&self) -> Value {
        let mut elems = Vec::with_capacity(1 + self.key.len());
        elems.push(Value::String(self.label.clone()));
        elems.extend(self.key.iter().cloned());
        Value::List(elems)
    }

    /// Reconstruct from the logical form produced by [`Self::to_value`].
    pub fn from_value(value: &Value) -> Result<Self, GraphKeyError> {
        let Value::List(elems) = value else {
            return Err(GraphKeyError::Shape("node key must be a list element"));
        };
        let Some((Value::String(label), key)) = elems.split_first() else {
            return Err(GraphKeyError::Shape(
                "node key list must start with a string label",
            ));
        };
        NodeKey::new(label.clone(), key.to_vec())
    }

    /// The canonical encoded form — the `nodes`-map key, and the exact
    /// bytes embedded in edge and index keys.
    pub fn encode(&self) -> Result<Vec<u8>, GraphKeyError> {
        Ok(keys::encode_key(std::slice::from_ref(&self.to_value()))?)
    }

    /// Decode a `nodes`-map key.
    pub fn decode(bytes: &[u8]) -> Result<Self, GraphKeyError> {
        let tuple = keys::decode_key(bytes)?;
        let [elem] = tuple.as_slice() else {
            return Err(GraphKeyError::Shape(
                "nodes-map key must be a single list element",
            ));
        };
        NodeKey::from_value(elem)
    }
}

/// Byte prefix of every `nodes`-map key whose primary label is `label`:
/// "all nodes with label L" is a prolly range scan over this prefix.
pub fn node_label_prefix(label: &str) -> Vec<u8> {
    keys::encode_list_prefix(std::slice::from_ref(&Value::String(label.to_owned())))
        .expect("string list prefixes always encode")
}

/// A relationship's identity:
/// `(source node key, type, target node key, discriminator)` (spec §2).
///
/// The discriminator is `Value::Null` unless the relationship type
/// declares a discriminator property in schema.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeKey {
    src: NodeKey,
    rtype: String,
    dst: NodeKey,
    disc: Value,
}

impl EdgeKey {
    /// Validate and construct an edge key. `disc` must be a scalar
    /// (`Null` for the default discriminator).
    pub fn new(
        src: NodeKey,
        rtype: impl Into<String>,
        dst: NodeKey,
        disc: Value,
    ) -> Result<Self, GraphKeyError> {
        let rtype = rtype.into();
        if rtype.is_empty() {
            return Err(GraphKeyError::EmptyName("relationship type"));
        }
        check_scalar(&disc, "discriminator")?;
        Ok(EdgeKey {
            src,
            rtype,
            dst,
            disc,
        })
    }

    /// The source node key.
    pub fn src(&self) -> &NodeKey {
        &self.src
    }

    /// The relationship type.
    pub fn rtype(&self) -> &str {
        &self.rtype
    }

    /// The target node key.
    pub fn dst(&self) -> &NodeKey {
        &self.dst
    }

    /// The discriminator (`Null` when defaulted).
    pub fn disc(&self) -> &Value {
        &self.disc
    }

    fn encode_tuple(&self, first: &NodeKey, second: &NodeKey) -> Result<Vec<u8>, GraphKeyError> {
        Ok(keys::encode_key(&[
            first.to_value(),
            Value::String(self.rtype.clone()),
            second.to_value(),
            self.disc.clone(),
        ])?)
    }

    /// The `edges_fwd` key: `[src, type, dst, disc]`.
    pub fn encode_fwd(&self) -> Result<Vec<u8>, GraphKeyError> {
        self.encode_tuple(&self.src, &self.dst)
    }

    /// The `edges_rev` key: `[dst, type, src, disc]` (spec §3.3).
    pub fn encode_rev(&self) -> Result<Vec<u8>, GraphKeyError> {
        self.encode_tuple(&self.dst, &self.src)
    }

    fn decode_tuple(bytes: &[u8]) -> Result<(NodeKey, String, NodeKey, Value), GraphKeyError> {
        let tuple = keys::decode_key(bytes)?;
        let [first, rtype, second, disc] = tuple.as_slice() else {
            return Err(GraphKeyError::Shape(
                "edge key must have exactly four elements",
            ));
        };
        let Value::String(rtype) = rtype else {
            return Err(GraphKeyError::Shape(
                "edge key second element must be a string type",
            ));
        };
        Ok((
            NodeKey::from_value(first)?,
            rtype.clone(),
            NodeKey::from_value(second)?,
            disc.clone(),
        ))
    }

    /// Decode an `edges_fwd` key.
    pub fn decode_fwd(bytes: &[u8]) -> Result<Self, GraphKeyError> {
        let (src, rtype, dst, disc) = Self::decode_tuple(bytes)?;
        EdgeKey::new(src, rtype, dst, disc)
    }

    /// Decode an `edges_rev` key (the first element is the target).
    pub fn decode_rev(bytes: &[u8]) -> Result<Self, GraphKeyError> {
        let (dst, rtype, src, disc) = Self::decode_tuple(bytes)?;
        EdgeKey::new(src, rtype, dst, disc)
    }
}

/// Byte prefix of every edge key whose leading endpoint is `node`: "all
/// edges out of X" on `edges_fwd`, "all edges into X" on `edges_rev`.
pub fn edge_endpoint_prefix(node: &NodeKey) -> Result<Vec<u8>, GraphKeyError> {
    node.encode()
}

/// Byte prefix of every edge key with leading endpoint `node` and type
/// `rtype`: "all T-edges out of / into X".
pub fn edge_endpoint_type_prefix(node: &NodeKey, rtype: &str) -> Result<Vec<u8>, GraphKeyError> {
    Ok(keys::encode_key(&[
        node.to_value(),
        Value::String(rtype.to_owned()),
    ])?)
}

/// One entry of a declared property index map `idx/<name>`:
/// `[String(label), List(property names), List(values), node key]` (spec §3.3).
/// The value is empty; the key is everything. A **composite** index carries
/// more than one property; a single-property index is the one-element case,
/// encoded uniformly (a one-element list, not a bare scalar).
///
/// This layer is policy-free about *which* values are indexable beyond what the
/// key encoding itself enforces (NaN is unencodable, ADR-0004); the graph layer
/// decides how null/NaN-valued properties are handled when it maintains
/// indexes.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexEntry {
    label: String,
    properties: Vec<String>,
    values: Vec<Value>,
    node: NodeKey,
}

impl IndexEntry {
    /// Validate and construct an index entry. `properties` must be non-empty
    /// (no empty names) and the same length as `values`.
    pub fn new(
        label: impl Into<String>,
        properties: Vec<String>,
        values: Vec<Value>,
        node: NodeKey,
    ) -> Result<Self, GraphKeyError> {
        let label = label.into();
        if label.is_empty() {
            return Err(GraphKeyError::EmptyName("index label"));
        }
        if properties.is_empty() {
            return Err(GraphKeyError::EmptyName("index property"));
        }
        if properties.iter().any(|p| p.is_empty()) {
            return Err(GraphKeyError::EmptyName("index property"));
        }
        if properties.len() != values.len() {
            return Err(GraphKeyError::Shape(
                "index entry property and value counts differ",
            ));
        }
        Ok(IndexEntry {
            label,
            properties,
            values,
            node,
        })
    }

    /// The indexed label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The indexed property names, in declaration order.
    pub fn properties(&self) -> &[String] {
        &self.properties
    }

    /// The indexed values, aligned with [`Self::properties`].
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// The node the entry points at.
    pub fn node(&self) -> &NodeKey {
        &self.node
    }

    /// The `idx/<name>` map key.
    pub fn encode(&self) -> Result<Vec<u8>, GraphKeyError> {
        Ok(keys::encode_key(&[
            Value::String(self.label.clone()),
            Value::List(self.properties.iter().cloned().map(Value::String).collect()),
            Value::List(self.values.clone()),
            self.node.to_value(),
        ])?)
    }

    /// Decode an `idx/<name>` map key.
    pub fn decode(bytes: &[u8]) -> Result<Self, GraphKeyError> {
        let tuple = keys::decode_key(bytes)?;
        let [label, properties, values, node] = tuple.as_slice() else {
            return Err(GraphKeyError::Shape(
                "index key must have exactly four elements",
            ));
        };
        let (Value::String(label), Value::List(properties), Value::List(values)) =
            (label, properties, values)
        else {
            return Err(GraphKeyError::Shape(
                "index key must be [label, [properties], [values], node]",
            ));
        };
        let properties = properties
            .iter()
            .map(|p| match p {
                Value::String(s) => Ok(s.clone()),
                _ => Err(GraphKeyError::Shape("index property names must be strings")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        IndexEntry::new(
            label.clone(),
            properties,
            values.clone(),
            NodeKey::from_value(node)?,
        )
    }
}

/// Byte prefix of every index entry for `(label, properties)` — the "all
/// entries of this index" scan and the base for a value-tuple seek prefix.
pub fn index_prefix(label: &str, properties: &[String]) -> Vec<u8> {
    keys::encode_key(&[
        Value::String(label.to_owned()),
        Value::List(properties.iter().cloned().map(Value::String).collect()),
    ])
    .expect("string tuples always encode")
}

/// Byte prefix of every index entry for `(label, property, value)` — an
/// equality probe; the remaining suffix enumerates matching node keys.
pub fn index_value_prefix(
    label: &str,
    properties: &[String],
    values: &[Value],
) -> Result<Vec<u8>, GraphKeyError> {
    Ok(keys::encode_key(&[
        Value::String(label.to_owned()),
        Value::List(properties.iter().cloned().map(Value::String).collect()),
        Value::List(values.to_vec()),
    ])?)
}

/// The smallest byte string strictly greater than every string with the
/// given prefix, for forming half-open scan ranges
/// `prefix..prefix_successor(prefix)`. `None` means the range is
/// unbounded above (the prefix is empty or all `0xff`).
pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut succ = prefix.to_vec();
    while let Some(&last) = succ.last() {
        if last == 0xff {
            succ.pop();
        } else {
            *succ.last_mut().expect("non-empty") = last + 1;
            return Some(succ);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nk(label: &str, key: &str) -> NodeKey {
        NodeKey::new(label, vec![Value::String(key.to_owned())]).expect("valid")
    }

    #[test]
    fn node_key_round_trips() {
        let k = NodeKey::new("Host", vec![Value::String("web1".into()), Value::Int(8080)])
            .expect("valid");
        let bytes = k.encode().expect("encode");
        assert_eq!(NodeKey::decode(&bytes).expect("decode"), k);
    }

    #[test]
    fn node_key_rejects_invalid_identity() {
        assert!(matches!(
            NodeKey::new("", vec![Value::Int(1)]),
            Err(GraphKeyError::EmptyName(_))
        ));
        assert!(matches!(
            NodeKey::new("Host", vec![]),
            Err(GraphKeyError::EmptyKeyTuple)
        ));
        assert!(matches!(
            NodeKey::new("Host", vec![Value::Null]),
            Err(GraphKeyError::NullKeyValue)
        ));
        assert!(matches!(
            NodeKey::new("Host", vec![Value::List(vec![])]),
            Err(GraphKeyError::NonScalar(_))
        ));
        assert!(matches!(
            NodeKey::new("Host", vec![Value::Float(f64::NAN)]),
            Err(GraphKeyError::NanKeyValue(_))
        ));
    }

    #[test]
    fn node_key_encoding_is_identical_standalone_and_embedded() {
        let src = nk("Host", "web1");
        let dst = nk("Service", "db");
        let standalone = src.encode().expect("encode");
        let edge = EdgeKey::new(src, "DEPENDS_ON", dst, Value::Null)
            .expect("valid")
            .encode_fwd()
            .expect("encode");
        assert!(
            edge.starts_with(&standalone),
            "edge key must embed the node key's exact standalone bytes"
        );
    }

    #[test]
    fn label_prefix_covers_exactly_that_label() {
        let prefix = node_label_prefix("Host");
        let host = nk("Host", "web1").encode().expect("encode");
        let hostile = nk("Hostile", "web1").encode().expect("encode");
        let service = nk("Service", "db").encode().expect("encode");
        assert!(host.starts_with(&prefix));
        // "Hostile" shares the string prefix "Host" but not the encoded
        // label element: chunked string framing closes the label.
        assert!(!hostile.starts_with(&prefix));
        assert!(!service.starts_with(&prefix));
    }

    #[test]
    fn edge_keys_round_trip_both_directions() {
        let e = EdgeKey::new(
            nk("Host", "web1"),
            "DEPENDS_ON",
            nk("Service", "db"),
            Value::Int(2),
        )
        .expect("valid");
        let fwd = e.encode_fwd().expect("encode");
        let rev = e.encode_rev().expect("encode");
        assert_eq!(EdgeKey::decode_fwd(&fwd).expect("decode"), e);
        assert_eq!(EdgeKey::decode_rev(&rev).expect("decode"), e);
        assert_ne!(fwd, rev);
    }

    #[test]
    fn edge_prefixes_group_by_endpoint_then_type() {
        let src = nk("Host", "web1");
        let e = EdgeKey::new(src.clone(), "DEPENDS_ON", nk("Service", "db"), Value::Null)
            .expect("valid");
        let fwd = e.encode_fwd().expect("encode");
        let by_endpoint = edge_endpoint_prefix(&src).expect("prefix");
        let by_type = edge_endpoint_type_prefix(&src, "DEPENDS_ON").expect("prefix");
        assert!(fwd.starts_with(&by_endpoint));
        assert!(fwd.starts_with(&by_type));
        assert!(by_type.starts_with(&by_endpoint));
        let other_type = edge_endpoint_type_prefix(&src, "HOSTS").expect("prefix");
        assert!(!fwd.starts_with(&other_type));
    }

    /// Gate D freeze-audit nit (acetone-093): relationship types sharing a
    /// prefix ("DEPENDS_ON" vs "DEPENDS_ON_MORE") must keep byte order ==
    /// logical order, and a type-scoped scan prefix must never capture an
    /// extension of the type — the chunked string framing closes the type
    /// element exactly.
    #[test]
    fn shared_prefix_rel_types_order_and_group_correctly() {
        let src = nk("Host", "web1");
        let dst = nk("Service", "db");
        let short =
            EdgeKey::new(src.clone(), "DEPENDS_ON", dst.clone(), Value::Null).expect("valid");
        let long =
            EdgeKey::new(src.clone(), "DEPENDS_ON_MORE", dst.clone(), Value::Null).expect("valid");
        let short_fwd = short.encode_fwd().expect("encode");
        let long_fwd = long.encode_fwd().expect("encode");
        assert!(
            short_fwd < long_fwd,
            "byte order must equal rel-type string order (prefix first)"
        );
        assert!(
            short.encode_rev().expect("encode") < long.encode_rev().expect("encode"),
            "reverse keys must order identically"
        );
        let prefix = edge_endpoint_type_prefix(&src, "DEPENDS_ON").expect("prefix");
        assert!(short_fwd.starts_with(&prefix));
        assert!(
            !long_fwd.starts_with(&prefix),
            "a DEPENDS_ON scan must not capture DEPENDS_ON_MORE edges"
        );
    }

    #[test]
    fn default_discriminator_sorts_first_within_group() {
        let src = nk("Host", "web1");
        let dst = nk("Service", "db");
        let default = EdgeKey::new(src.clone(), "T", dst.clone(), Value::Null)
            .expect("valid")
            .encode_fwd()
            .expect("encode");
        for disc in [
            Value::Bool(false),
            Value::Int(i64::MIN),
            Value::String(String::new()),
        ] {
            let keyed = EdgeKey::new(src.clone(), "T", dst.clone(), disc)
                .expect("valid")
                .encode_fwd()
                .expect("encode");
            assert!(default < keyed, "Null discriminator must sort first");
        }
    }

    #[test]
    fn index_entry_round_trips_and_prefixes_nest() {
        // Single-property index: a one-element property/value list.
        let entry = IndexEntry::new(
            "Host",
            vec!["region".into()],
            vec![Value::String("eu".into())],
            nk("Host", "web1"),
        )
        .expect("valid");
        let bytes = entry.encode().expect("encode");
        assert_eq!(IndexEntry::decode(&bytes).expect("decode"), entry);
        let props = vec!["region".to_string()];
        let by_prop = index_prefix("Host", &props);
        let by_value =
            index_value_prefix("Host", &props, &[Value::String("eu".into())]).expect("prefix");
        assert!(bytes.starts_with(&by_prop));
        assert!(bytes.starts_with(&by_value));
        assert!(by_value.starts_with(&by_prop));
    }

    #[test]
    fn composite_index_entry_round_trips_and_seek_prefix_nests() {
        let props = vec!["os".to_string(), "dc".to_string()];
        let vals = vec![Value::String("linux".into()), Value::Int(3)];
        let entry = IndexEntry::new("Host", props.clone(), vals.clone(), nk("Host", "web1"))
            .expect("valid");
        let bytes = entry.encode().expect("encode");
        let back = IndexEntry::decode(&bytes).expect("decode");
        assert_eq!(back, entry);
        assert_eq!(back.properties(), props.as_slice());
        assert_eq!(back.values(), vals.as_slice());
        // Seeking the full value tuple is a byte prefix of the entry.
        let by_value = index_value_prefix("Host", &props, &vals).expect("prefix");
        assert!(bytes.starts_with(&by_value));
        assert!(by_value.starts_with(&index_prefix("Host", &props)));
        // Arity mismatch is rejected.
        assert!(
            IndexEntry::new("Host", props.clone(), vec![Value::Int(3)], nk("Host", "w")).is_err()
        );
    }

    #[test]
    fn prefix_successor_bounds() {
        assert_eq!(prefix_successor(&[0x01, 0x02]), Some(vec![0x01, 0x03]));
        assert_eq!(prefix_successor(&[0x01, 0xff]), Some(vec![0x02]));
        assert_eq!(prefix_successor(&[0xff, 0xff]), None);
        assert_eq!(prefix_successor(&[]), None);
    }
}
