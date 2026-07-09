//! Property tests for the spec §3.3 map layouts (ADR-0008): graph keys,
//! records, schema entries and the manifest. Round trips, canonical
//! re-encoding, prefix/grouping behaviour, and decoder totality on
//! hostile bytes (Load-Bearing Invariants 2 and 3).

use acetone_model::graph_keys::{
    EdgeKey, IndexEntry, NodeKey, edge_endpoint_prefix, edge_endpoint_type_prefix,
    index_value_prefix, node_label_prefix, prefix_successor,
};
use acetone_model::manifest::{FORMAT_VERSION, Manifest, ManifestDecodeError, MapRoot};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{
    IndexDef, LabelDef, PropertyType, RelTypeDef, SchemaEntry, schema_kind_prefix,
};
use acetone_model::{Date, Time, Value};
use acetone_prolly::{ChunkParams, Hash};
use proptest::prelude::*;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Non-empty names for labels, types, properties and indexes.
fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => "[A-Za-z_][A-Za-z0-9_]{0,10}",
        1 => "\\PC{1,6}", // arbitrary non-control unicode
    ]
}

/// Scalars valid in node-key tuples: non-null, non-list, non-NaN.
fn key_scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        3 => any::<i64>().prop_map(Value::Int),
        3 => ".{0,16}".prop_map(Value::String),
        1 => any::<bool>().prop_map(Value::Bool),
        1 => any::<f64>().prop_map(|x| {
            let x = if x.is_nan() { 0.5 } else { x };
            Value::Float(if x == 0.0 { 0.0 } else { x })
        }),
        1 => any::<i64>().prop_map(|d| Value::Date(Date { days: d })),
        1 => (0..86_400_000_000_000u64).prop_map(|n| Value::Time(Time { nanos: n })),
        1 => proptest::collection::vec(any::<u8>(), 0..12).prop_map(Value::Bytes),
    ]
}

/// Discriminators: the default (`Null`) or a scalar.
fn discriminator() -> impl Strategy<Value = Value> {
    prop_oneof![
        2 => Just(Value::Null),
        1 => key_scalar(),
    ]
}

fn node_key() -> impl Strategy<Value = NodeKey> {
    (name(), proptest::collection::vec(key_scalar(), 1..4))
        .prop_map(|(label, key)| NodeKey::new(label, key).expect("strategy yields valid keys"))
}

fn edge_key() -> impl Strategy<Value = EdgeKey> {
    (node_key(), name(), node_key(), discriminator()).prop_map(|(src, rtype, dst, disc)| {
        EdgeKey::new(src, rtype, dst, disc).expect("strategy yields valid keys")
    })
}

/// Property values: anything the value encoding accepts except NaN
/// (NaN payload collapse is covered by the values suite; excluding it
/// here keeps derived equality usable in round-trip assertions).
fn property_value() -> impl Strategy<Value = Value> {
    let scalar = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        any::<f64>().prop_map(|x| Value::Float(if x.is_nan() { 0.5 } else { x })),
        ".{0,12}".prop_map(Value::String),
        proptest::collection::vec(any::<u8>(), 0..8).prop_map(Value::Bytes),
    ];
    scalar.prop_recursive(3, 24, 4, |inner| {
        proptest::collection::vec(inner, 0..4)
            .prop_map(Value::List)
            .boxed()
    })
}

fn properties() -> impl Strategy<Value = BTreeMap<String, Value>> {
    proptest::collection::btree_map(name(), property_value(), 0..5)
}

fn node_record() -> impl Strategy<Value = NodeRecord> {
    (proptest::collection::vec(name(), 0..4), properties())
        .prop_map(|(labels, props)| NodeRecord::new(labels, props))
}

fn property_types() -> impl Strategy<Value = BTreeMap<String, PropertyType>> {
    let ty = prop_oneof![
        Just(PropertyType::Bool),
        Just(PropertyType::Int),
        Just(PropertyType::Float),
        Just(PropertyType::String),
        Just(PropertyType::Bytes),
        Just(PropertyType::Date),
        Just(PropertyType::Time),
        Just(PropertyType::DateTime),
        Just(PropertyType::Duration),
        Just(PropertyType::List),
    ];
    proptest::collection::btree_map(name(), ty, 0..4)
}

fn label_entry() -> impl Strategy<Value = SchemaEntry> {
    (
        name(),
        proptest::collection::btree_set(name(), 1..4),
        property_types(),
        proptest::collection::vec(name(), 0..3),
        proptest::collection::vec(name(), 0..3),
        any::<bool>(),
    )
        .prop_map(|(entry_name, key_set, types, exists, unique, surrogate)| {
            let key: Vec<String> = key_set.into_iter().collect();
            let unique: Vec<String> = unique.into_iter().filter(|u| !key.contains(u)).collect();
            let def = if surrogate {
                LabelDef::surrogate(types, exists, unique)
            } else {
                LabelDef::new(key, types, exists, unique)
            }
            .expect("strategy yields valid definitions");
            SchemaEntry::Label {
                name: entry_name,
                def,
            }
        })
}

fn schema_entry() -> impl Strategy<Value = SchemaEntry> {
    prop_oneof![
        2 => label_entry(),
        1 => (name(), proptest::option::of(name()), property_types(),
              proptest::collection::vec(name(), 0..3))
            .prop_map(|(entry_name, disc, types, exists)| SchemaEntry::RelType {
                name: entry_name,
                def: RelTypeDef::new(disc, types, exists).expect("valid"),
            }),
        1 => (name(), name(), proptest::collection::vec(name(), 1..4))
            .prop_map(|(entry_name, label, properties)| SchemaEntry::Index {
                name: entry_name,
                // 1..4 properties exercises both single and composite indexes.
                def: IndexDef::new(label, properties).expect("valid"),
            }),
    ]
}

fn map_root() -> impl Strategy<Value = MapRoot> {
    let hash_bytes = prop_oneof![
        proptest::collection::vec(any::<u8>(), 20..=20), // SHA-1 width
        proptest::collection::vec(any::<u8>(), 32..=32), // SHA-256 width
    ];
    (hash_bytes, 1u32..=64).prop_map(|(bytes, height)| MapRoot {
        hash: Hash::from_bytes(&bytes).expect("20/32 bytes is a valid width"),
        height,
    })
}

fn manifest() -> impl Strategy<Value = Manifest> {
    (
        map_root(),
        map_root(),
        map_root(),
        map_root(),
        proptest::collection::btree_map(name(), map_root(), 0..4),
        proptest::option::of(map_root()),
        proptest::sample::select(vec![
            (256u32, 8u32, 4096u32),
            (1024, 12, 65536),
            (64, 4, 1024),
        ]),
    )
        .prop_map(
            |(schema, nodes, edges_fwd, edges_rev, indexes, conflicts, (min, mask, max))| {
                Manifest {
                    chunk_params: ChunkParams::new(min, mask, max).expect("valid params"),
                    schema,
                    nodes,
                    edges_fwd,
                    edges_rev,
                    indexes,
                    conflicts,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// Bit-exact equality (aliasing tests)
// ---------------------------------------------------------------------------

/// Value equality by encoded identity: floats compare by bit pattern, so
/// `+0.0 != -0.0` (distinct canonical encodings) and NaN equals itself.
/// Derived `PartialEq` follows IEEE semantics and cannot make these
/// distinctions, which is what aliasing tests must judge (acetone-9rw).
fn value_eq_bitexact(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::List(xs), Value::List(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| value_eq_bitexact(x, y))
        }
        _ => a == b,
    }
}

/// Record equality under [`value_eq_bitexact`].
fn record_eq_bitexact(a: &NodeRecord, b: &NodeRecord) -> bool {
    a.secondary_labels() == b.secondary_labels()
        && a.properties().len() == b.properties().len()
        && a.properties()
            .iter()
            .zip(b.properties())
            .all(|((ka, va), (kb, vb))| ka == kb && value_eq_bitexact(va, vb))
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    /// Node keys: round trip, and the standalone encoding is byte-for-byte
    /// the prefix a forward edge key starts with (one encoding of identity
    /// everywhere — ADR-0008).
    #[test]
    fn node_key_round_trip_and_embedding(k in node_key(), e in edge_key()) {
        let bytes = k.encode().expect("encodable");
        prop_assert_eq!(NodeKey::decode(&bytes).expect("decodable"), k);
        let src_bytes = e.src().encode().expect("encodable");
        prop_assert!(e.encode_fwd().expect("encodable").starts_with(&src_bytes));
        let dst_bytes = e.dst().encode().expect("encodable");
        prop_assert!(e.encode_rev().expect("encodable").starts_with(&dst_bytes));
    }

    /// Node keys group exactly by label: an encoded key starts with a
    /// label's prefix if and only if it bears that label.
    #[test]
    fn label_prefix_matches_exactly(k in node_key(), other in name()) {
        let bytes = k.encode().expect("encodable");
        prop_assert!(bytes.starts_with(&node_label_prefix(k.label())));
        prop_assert_eq!(
            bytes.starts_with(&node_label_prefix(&other)),
            k.label() == other,
            "prefix match must equal label equality (label {:?} vs {:?})",
            k.label(), other
        );
    }

    /// Edge keys: round trip through both map layouts, and prefixes nest
    /// (endpoint ⊂ endpoint+type ⊂ full key).
    #[test]
    fn edge_key_round_trip_and_prefixes(e in edge_key()) {
        let fwd = e.encode_fwd().expect("encodable");
        let rev = e.encode_rev().expect("encodable");
        prop_assert_eq!(&EdgeKey::decode_fwd(&fwd).expect("decodable"), &e);
        prop_assert_eq!(&EdgeKey::decode_rev(&rev).expect("decodable"), &e);
        let by_src = edge_endpoint_prefix(e.src()).expect("encodable");
        let by_src_type = edge_endpoint_type_prefix(e.src(), e.rtype()).expect("encodable");
        prop_assert!(fwd.starts_with(&by_src));
        prop_assert!(fwd.starts_with(&by_src_type));
        prop_assert!(by_src_type.starts_with(&by_src));
    }

    /// Index entries: round trip; the equality-probe prefix matches.
    #[test]
    fn index_entry_round_trip(label in name(), property in name(), v in key_scalar(), k in node_key()) {
        let entry = IndexEntry::new(label.clone(), vec![property.clone()], vec![v.clone()], k)
            .expect("valid");
        let bytes = entry.encode().expect("encodable");
        prop_assert_eq!(IndexEntry::decode(&bytes).expect("decodable"), entry);
        let probe = index_value_prefix(&label, std::slice::from_ref(&property), std::slice::from_ref(&v)).expect("encodable");
        prop_assert!(bytes.starts_with(&probe));
    }

    /// Node records: round trip and byte-identical re-encoding.
    #[test]
    fn node_record_round_trip(rec in node_record()) {
        let bytes = rec.encode().expect("encodable");
        let back = NodeRecord::decode(&bytes).expect("decodable");
        prop_assert_eq!(&back, &rec);
        prop_assert_eq!(back.encode().expect("encodable"), bytes);
    }

    /// Edge records: round trip and byte-identical re-encoding.
    #[test]
    fn edge_record_round_trip(props in properties()) {
        let rec = EdgeRecord::new(props);
        let bytes = rec.encode().expect("encodable");
        let back = EdgeRecord::decode(&bytes).expect("decodable");
        prop_assert_eq!(&back, &rec);
        prop_assert_eq!(back.encode().expect("encodable"), bytes);
    }

    /// Schema entries: round trip through (map key, value) and
    /// byte-identical re-encoding; the kind prefix always matches.
    #[test]
    fn schema_entry_round_trip(entry in schema_entry()) {
        let key = entry.map_key();
        let value = entry.encode_value();
        let back = SchemaEntry::decode(&key, &value).expect("decodable");
        prop_assert_eq!(&back, &entry);
        prop_assert_eq!(back.encode_value(), value);
        let kind = match &entry {
            SchemaEntry::Label { .. } => acetone_model::schema::KIND_LABEL,
            SchemaEntry::RelType { .. } => acetone_model::schema::KIND_RTYPE,
            SchemaEntry::Index { .. } => acetone_model::schema::KIND_INDEX,
        };
        prop_assert!(key.starts_with(&schema_kind_prefix(kind)));
    }

    /// Manifests: round trip, byte-identical re-encoding, and encoding is
    /// a pure function (two encodes agree) — "manifest hashing
    /// deterministic".
    #[test]
    fn manifest_round_trip_and_determinism(m in manifest()) {
        let bytes = m.encode();
        prop_assert_eq!(m.encode(), bytes.clone());
        let back = Manifest::decode(&bytes).expect("decodable");
        prop_assert_eq!(&back, &m);
        prop_assert_eq!(back.encode(), bytes);
    }

    /// Every truncation of a valid manifest fails to decode (no silent
    /// partial reads), and never panics.
    #[test]
    fn manifest_truncation_always_errors(m in manifest(), cut in any::<prop::sample::Index>()) {
        let bytes = m.encode();
        let len = cut.index(bytes.len());
        prop_assert!(Manifest::decode(&bytes[..len]).is_err());
    }

    /// Decoder totality: arbitrary bytes never panic any of the new
    /// decoders (they are fed untrusted repository data).
    #[test]
    fn decoders_are_total_on_arbitrary_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..96)) {
        let _ = NodeKey::decode(&bytes);
        let _ = EdgeKey::decode_fwd(&bytes);
        let _ = EdgeKey::decode_rev(&bytes);
        let _ = IndexEntry::decode(&bytes);
        let _ = NodeRecord::decode(&bytes);
        let _ = EdgeRecord::decode(&bytes);
        let _ = SchemaEntry::decode(&bytes, &bytes);
        let _ = Manifest::decode(&bytes);
    }

    /// Strict canonicity under mutation: flipping one bit of a valid
    /// record either fails to decode or decodes to a different record —
    /// each record has exactly one accepted byte form.
    ///
    /// "Different" must be judged **bit-exactly**: derived `PartialEq`
    /// follows IEEE semantics where `-0.0 == 0.0`, but `+0.0` and `-0.0`
    /// are distinct values with distinct canonical encodings (`f9 0000`
    /// vs `f9 8000` — zero sign is preserved in values, ADR-0004), so a
    /// sign-bit flip produces a genuinely different record that derived
    /// equality cannot see (acetone-9rw).
    #[test]
    fn record_mutation_never_aliases(rec in node_record(), idx in any::<prop::sample::Index>(), bit in 0u8..8) {
        let bytes = rec.encode().expect("encodable");
        prop_assume!(!bytes.is_empty());
        let mut mutated = bytes.clone();
        let i = idx.index(mutated.len());
        mutated[i] ^= 1 << bit;
        if let Ok(back) = NodeRecord::decode(&mutated) {
            prop_assert!(
                !record_eq_bitexact(&back, &rec),
                "mutated bytes decoded to the same record"
            );
        }
    }

    /// prefix_successor really is an exclusive upper bound for the prefix
    /// range: every extension of the prefix sorts below it, and nothing
    /// with a different prefix falls inside [prefix, successor).
    #[test]
    fn prefix_successor_bounds_extensions(prefix in proptest::collection::vec(any::<u8>(), 0..12),
                                          ext in proptest::collection::vec(any::<u8>(), 0..12)) {
        let mut extended = prefix.clone();
        extended.extend_from_slice(&ext);
        match prefix_successor(&prefix) {
            Some(succ) => {
                prop_assert!(extended < succ);
                prop_assert!(succ > prefix);
            }
            None => {
                // Unbounded: prefix is empty or all 0xff.
                prop_assert!(prefix.iter().all(|&b| b == 0xff));
            }
        }
    }
}

/// The version gate: a manifest from a future format is rejected by
/// version, before any body parsing.
#[test]
fn future_manifest_is_rejected_by_version() {
    let future = u64::from(FORMAT_VERSION) + 1;
    // [FORMAT_VERSION + 1, <garbage body that is not even CBOR>]
    let mut bytes = vec![0x82];
    bytes.push(future as u8); // small uint head
    bytes.extend_from_slice(&[0xde, 0xad]);
    assert_eq!(
        Manifest::decode(&bytes),
        Err(ManifestDecodeError::UnsupportedVersion(future))
    );
}
