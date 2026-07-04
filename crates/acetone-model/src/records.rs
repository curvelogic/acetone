//! Node and edge record encodings (spec §3.3, ADR-0008).
//!
//! The values stored against the graph-key layouts of
//! [`crate::graph_keys`]:
//!
//! - a **node record** ([`NodeRecord`]) is the canonical CBOR array
//!   `[secondary_labels, properties]` — secondary labels as a sorted,
//!   deduplicated text array, properties as a deterministic map;
//! - an **edge record** ([`EdgeRecord`]) is the properties map directly.
//!
//! Records are the hot path, so they use positional structure rather
//! than field-named maps (ADR-0008); schema entries and the manifest,
//! which are cold and tiny, use text-keyed maps instead.
//!
//! **Key properties are excluded from node records** (ADR-0008): key
//! values live solely in the map key, so a record that disagrees with
//! its key is unrepresentable (Load-Bearing Invariant 3). This layer
//! cannot know which property names are key columns — enforcing the
//! exclusion and recombining on read are graph-layer obligations.
//!
//! Both orderings (labels, property names) follow the bytewise order of
//! the canonical text encodings (RFC 8949 §4.2.1) — shorter strings
//! first, equal lengths bytewise. Encoders normalise; decoders are
//! strict, so `decode(bytes)` succeeding implies re-encoding is
//! byte-identical. Any change here is a `format_version` bump.

use crate::Value;
use crate::cbor::{MAJOR_ARRAY, MAJOR_MAP, Reader, canonical_str_cmp, write_head, write_text};
use crate::values::{self, ValueDecodeError, ValueEncodeError};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use thiserror::Error;

/// Errors from encoding a record.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RecordEncodeError {
    /// A property value was rejected by the value encoding.
    #[error(transparent)]
    Value(#[from] ValueEncodeError),
}

/// Errors from decoding a record.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RecordDecodeError {
    /// A low-level CBOR failure (truncation, non-canonical form, ...).
    #[error(transparent)]
    Cbor(#[from] ValueDecodeError),
    /// The outer structure was not the expected record shape.
    #[error("unexpected record shape: {0}")]
    Shape(&'static str),
    /// Labels or property names not in canonical order, or duplicated.
    #[error("record field order not canonical: {0}")]
    NotCanonical(&'static str),
}

/// Encode string items in canonical map-key order, rejecting nothing:
/// input is normalised (sorted, deduplicated) rather than validated.
fn normalise_strings(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = items.into_iter().collect();
    v.sort_by(|a, b| canonical_str_cmp(a, b));
    v.dedup();
    v
}

/// Write a deterministic CBOR map of property name → value.
fn write_properties(
    out: &mut Vec<u8>,
    properties: &BTreeMap<String, Value>,
) -> Result<(), RecordEncodeError> {
    // BTreeMap iterates in String order; canonical CBOR order is
    // length-first. Re-sort the (borrowed) entries.
    let mut entries: Vec<(&String, &Value)> = properties.iter().collect();
    entries.sort_by(|a, b| canonical_str_cmp(a.0, b.0));
    write_head(out, MAJOR_MAP, entries.len() as u64);
    for (name, value) in entries {
        write_text(out, name);
        values::write_value(out, value, 0)?;
    }
    Ok(())
}

/// Read a deterministic CBOR map of property name → value, enforcing
/// strictly ascending canonical key order (which also rules out
/// duplicates).
fn read_properties(reader: &mut Reader) -> Result<BTreeMap<String, Value>, RecordDecodeError> {
    let count = reader.read_head(MAJOR_MAP)?;
    // Each entry takes at least two bytes; bound allocation by input.
    if count > reader.remaining() as u64 {
        return Err(RecordDecodeError::Cbor(ValueDecodeError::LengthOverrun {
            declared: count,
            remaining: reader.remaining(),
        }));
    }
    let mut properties = BTreeMap::new();
    let mut previous: Option<String> = None;
    for _ in 0..count {
        let name = reader.read_text()?;
        if let Some(prev) = &previous
            && canonical_str_cmp(prev, &name) != Ordering::Less
        {
            return Err(RecordDecodeError::NotCanonical(
                "property names must be strictly ascending",
            ));
        }
        let value = values::read_value(reader, 0)?;
        previous = Some(name.clone());
        properties.insert(name, value);
    }
    Ok(properties)
}

/// A node's stored state: secondary labels and non-key properties
/// (spec §3.3; key properties are excluded per ADR-0008).
///
/// Secondary labels are held sorted and deduplicated (canonical text
/// order) by construction, so structural equality equals encoded
/// equality.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRecord {
    secondary_labels: Vec<String>,
    properties: BTreeMap<String, Value>,
}

impl NodeRecord {
    /// Construct a record, normalising the label set.
    pub fn new(
        secondary_labels: impl IntoIterator<Item = String>,
        properties: BTreeMap<String, Value>,
    ) -> Self {
        NodeRecord {
            secondary_labels: normalise_strings(secondary_labels),
            properties,
        }
    }

    /// The secondary labels, sorted and deduplicated.
    pub fn secondary_labels(&self) -> &[String] {
        &self.secondary_labels
    }

    /// The non-key properties.
    pub fn properties(&self) -> &BTreeMap<String, Value> {
        &self.properties
    }

    /// Encode as canonical CBOR `[secondary_labels, properties]`.
    pub fn encode(&self) -> Result<Vec<u8>, RecordEncodeError> {
        let mut out = Vec::new();
        write_head(&mut out, MAJOR_ARRAY, 2);
        write_head(&mut out, MAJOR_ARRAY, self.secondary_labels.len() as u64);
        for label in &self.secondary_labels {
            write_text(&mut out, label);
        }
        write_properties(&mut out, &self.properties)?;
        Ok(out)
    }

    /// Decode, strictly: exactly the bytes [`Self::encode`] produces.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordDecodeError> {
        let mut reader = Reader::new(bytes);
        let arity = reader.read_head(MAJOR_ARRAY)?;
        if arity != 2 {
            return Err(RecordDecodeError::Shape(
                "node record must be a two-element array",
            ));
        }
        let count = reader.read_head(MAJOR_ARRAY)?;
        if count > reader.remaining() as u64 {
            return Err(RecordDecodeError::Cbor(ValueDecodeError::LengthOverrun {
                declared: count,
                remaining: reader.remaining(),
            }));
        }
        let mut secondary_labels: Vec<String> = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let label = reader.read_text()?;
            if let Some(prev) = secondary_labels.last()
                && canonical_str_cmp(prev, &label) != Ordering::Less
            {
                return Err(RecordDecodeError::NotCanonical(
                    "secondary labels must be strictly ascending",
                ));
            }
            secondary_labels.push(label);
        }
        let properties = read_properties(&mut reader)?;
        if reader.remaining() != 0 {
            return Err(RecordDecodeError::Cbor(ValueDecodeError::TrailingBytes));
        }
        Ok(NodeRecord {
            secondary_labels,
            properties,
        })
    }
}

/// A relationship's stored state: its properties (spec §3.3).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EdgeRecord {
    properties: BTreeMap<String, Value>,
}

impl EdgeRecord {
    /// Construct a record.
    pub fn new(properties: BTreeMap<String, Value>) -> Self {
        EdgeRecord { properties }
    }

    /// The properties.
    pub fn properties(&self) -> &BTreeMap<String, Value> {
        &self.properties
    }

    /// Encode as a canonical CBOR properties map.
    pub fn encode(&self) -> Result<Vec<u8>, RecordEncodeError> {
        let mut out = Vec::new();
        write_properties(&mut out, &self.properties)?;
        Ok(out)
    }

    /// Decode, strictly: exactly the bytes [`Self::encode`] produces.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordDecodeError> {
        let mut reader = Reader::new(bytes);
        let properties = read_properties(&mut reader)?;
        if reader.remaining() != 0 {
            return Err(RecordDecodeError::Cbor(ValueDecodeError::TrailingBytes));
        }
        Ok(EdgeRecord { properties })
    }
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

    #[test]
    fn node_record_round_trips() {
        let rec = NodeRecord::new(
            ["Router".to_owned(), "Edge".to_owned()],
            props(&[
                ("region", Value::String("eu-west".into())),
                ("cores", Value::Int(8)),
                ("load", Value::Float(0.75)),
            ]),
        );
        let bytes = rec.encode().expect("encode");
        let back = NodeRecord::decode(&bytes).expect("decode");
        assert_eq!(back, rec);
        assert_eq!(back.encode().expect("re-encode"), bytes);
    }

    #[test]
    fn empty_node_record_is_canonical() {
        let rec = NodeRecord::new([], BTreeMap::new());
        let bytes = rec.encode().expect("encode");
        // [[], {}] — array(2), array(0), map(0).
        assert_eq!(bytes, vec![0x82, 0x80, 0xa0]);
        assert_eq!(NodeRecord::decode(&bytes).expect("decode"), rec);
    }

    #[test]
    fn secondary_labels_are_normalised() {
        let rec = NodeRecord::new(
            [
                "bb".to_owned(),
                "z".to_owned(),
                "bb".to_owned(),
                "a".to_owned(),
            ],
            BTreeMap::new(),
        );
        // Canonical order is length-first: "a", "z", "bb".
        assert_eq!(rec.secondary_labels(), ["a", "z", "bb"]);
    }

    #[test]
    fn property_keys_encode_in_canonical_order_not_string_order() {
        // String order would put "aa" before "z"; canonical order is the
        // reverse. The encoded bytes must use canonical order.
        let rec = EdgeRecord::new(props(&[("aa", Value::Int(1)), ("z", Value::Int(2))]));
        let bytes = rec.encode().expect("encode");
        // map(2), "z":1, "aa":1 → a2 61 7a 02 62 61 61 01
        assert_eq!(bytes, vec![0xa2, 0x61, 0x7a, 0x02, 0x62, 0x61, 0x61, 0x01]);
        assert_eq!(EdgeRecord::decode(&bytes).expect("decode"), rec);
    }

    #[test]
    fn decoder_rejects_unsorted_and_duplicate_keys() {
        // map(2) with keys in string order ("aa" then "z") — non-canonical.
        let unsorted = vec![0xa2, 0x62, 0x61, 0x61, 0x01, 0x61, 0x7a, 0x02];
        assert!(matches!(
            EdgeRecord::decode(&unsorted),
            Err(RecordDecodeError::NotCanonical(_))
        ));
        // map(2) with a duplicated key.
        let duplicate = vec![0xa2, 0x61, 0x7a, 0x01, 0x61, 0x7a, 0x02];
        assert!(matches!(
            EdgeRecord::decode(&duplicate),
            Err(RecordDecodeError::NotCanonical(_))
        ));
    }

    #[test]
    fn decoder_rejects_unsorted_labels_and_trailing_bytes() {
        // [["bb","a"], {}] — labels out of canonical order.
        let unsorted = vec![0x82, 0x82, 0x62, 0x62, 0x62, 0x61, 0x61, 0xa0];
        assert!(matches!(
            NodeRecord::decode(&unsorted),
            Err(RecordDecodeError::NotCanonical(_))
        ));
        let mut trailing = NodeRecord::new([], BTreeMap::new())
            .encode()
            .expect("encode");
        trailing.push(0x00);
        assert!(matches!(
            NodeRecord::decode(&trailing),
            Err(RecordDecodeError::Cbor(ValueDecodeError::TrailingBytes))
        ));
    }

    #[test]
    fn hostile_declared_counts_do_not_allocate() {
        // array(2), array with declared length 2^32 — must error, not OOM.
        let hostile = vec![0x82, 0x9a, 0xff, 0xff, 0xff, 0xff];
        assert!(NodeRecord::decode(&hostile).is_err());
        let hostile_map = vec![0xba, 0xff, 0xff, 0xff, 0xff];
        assert!(EdgeRecord::decode(&hostile_map).is_err());
    }
}
