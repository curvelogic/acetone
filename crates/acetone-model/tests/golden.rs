//! Golden byte-exact fixtures for the normative spec §3.4 encodings.
//!
//! These vectors are the **format_version 1 fixtures**. They pin the
//! on-disk byte form of keys and values; every root hash in every acetone
//! repository ultimately depends on them.
//!
//! **Changing any expected byte string here is a format change**: per
//! CLAUDE.md Load-Bearing Invariant 2 and spec §10 it requires a
//! `format_version` bump in the manifest header (and, pre-1.0, an
//! `acetone migrate` history rewrite). Do not "fix" a failing golden test
//! by updating the fixture unless that is a deliberate, ADR-recorded
//! format revision.
//!
//! CBOR fixtures for primitives are cross-checked against RFC 8949
//! Appendix A; temporal and key fixtures were computed independently of
//! the implementation.

use acetone_model::graph_keys::{EdgeKey, IndexEntry, NodeKey, node_label_prefix};
use acetone_model::manifest::{Manifest, MapRoot};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{
    IndexDef, LabelDef, PropertyType, RelTypeDef, SchemaEntry, schema_kind_prefix,
};
use acetone_model::{Date, DateTime, Duration, Time, Value};
use acetone_model::{keys, values};
use acetone_prolly::{ChunkParams, Hash};
use std::collections::BTreeMap;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex literal");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

#[track_caller]
fn assert_value_golden(value: Value, expected_hex: &str) {
    let encoded = values::encode_value(&value).expect("encode");
    assert_eq!(
        hex(&encoded),
        expected_hex,
        "value encoding changed for {value:?} — this is a format_version bump"
    );
    let decoded = values::decode_value(&unhex(expected_hex)).expect("decode");
    assert_eq!(values::encode_value(&decoded).unwrap(), encoded);
}

#[track_caller]
fn assert_key_golden(tuple: &[Value], expected_hex: &str) {
    let encoded = keys::encode_key(tuple).expect("encode");
    assert_eq!(
        hex(&encoded),
        expected_hex,
        "key encoding changed for {tuple:?} — this is a format_version bump"
    );
    let decoded = keys::decode_key(&unhex(expected_hex)).expect("decode");
    assert_eq!(decoded, tuple);
}

#[test]
fn golden_values_scalars() {
    assert_value_golden(Value::Null, "f6");
    assert_value_golden(Value::Bool(false), "f4");
    assert_value_golden(Value::Bool(true), "f5");

    assert_value_golden(Value::Int(0), "00");
    assert_value_golden(Value::Int(42), "182a");
    assert_value_golden(Value::Int(-42), "3829");
    assert_value_golden(Value::Int(1_000_000), "1a000f4240");
    assert_value_golden(Value::Int(i64::MAX), "1b7fffffffffffffff");
    assert_value_golden(Value::Int(i64::MIN), "3b7fffffffffffffff");

    assert_value_golden(Value::Float(0.0), "f90000");
    assert_value_golden(Value::Float(-0.0), "f98000");
    assert_value_golden(Value::Float(0.5), "f93800");
    assert_value_golden(Value::Float(100_000.0), "fa47c35000");
    assert_value_golden(Value::Float(std::f64::consts::PI), "fb400921fb54442d18");
    assert_value_golden(Value::Float(f64::INFINITY), "f97c00");
    assert_value_golden(Value::Float(f64::NEG_INFINITY), "f9fc00");
    assert_value_golden(Value::Float(f64::NAN), "f97e00");

    assert_value_golden(Value::String(String::new()), "60");
    assert_value_golden(Value::String("IETF".into()), "6449455446");
    assert_value_golden(Value::String("\u{6c34}".into()), "63e6b0b4");

    assert_value_golden(Value::Bytes(vec![]), "40");
    assert_value_golden(Value::Bytes(vec![1, 2, 3, 4]), "4401020304");
}

#[test]
fn golden_values_temporal() {
    // 2022-01-01 is 18993 days after the epoch.
    assert_value_golden(Value::Date(Date { days: 18993 }), "d864194a31");
    // 1969-12-31.
    assert_value_golden(Value::Date(Date { days: -1 }), "d86420");
    // Noon: 43_200_000_000_000 ns.
    assert_value_golden(
        Value::Time(Time {
            nanos: 43_200_000_000_000,
        }),
        "da000121741b0000274a48a78000",
    );
    assert_value_golden(Value::Time(Time { nanos: 0 }), "da0001217400");
    // 2023-11-14T22:13:20Z as 1_700_000_000_000_000_000 ns, offset +01:00.
    assert_value_golden(
        Value::DateTime(DateTime {
            epoch_nanos: 1_700_000_000_000_000_000,
            offset_minutes: 60,
        }),
        "da00012175821b17979cfe362a0000183c",
    );
    assert_value_golden(
        Value::DateTime(DateTime {
            epoch_nanos: 0,
            offset_minutes: 0,
        }),
        "da00012175820000",
    );
    assert_value_golden(
        Value::Duration(Duration {
            months: 1,
            days: 2,
            nanos: 3,
        }),
        "da0001217683010203",
    );
    assert_value_golden(
        Value::Duration(Duration {
            months: -1,
            days: -2,
            nanos: -3,
        }),
        "da0001217683202122",
    );
}

#[test]
fn golden_values_lists() {
    assert_value_golden(Value::List(vec![]), "80");
    assert_value_golden(
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
        "83010203",
    );
    assert_value_golden(
        Value::List(vec![
            Value::String("a".into()),
            Value::List(vec![Value::Bool(true), Value::Null]),
        ]),
        "82616182f5f6",
    );
}

#[test]
fn golden_keys_scalars() {
    assert_key_golden(&[Value::Null], "01");
    assert_key_golden(&[Value::Bool(false)], "02");
    assert_key_golden(&[Value::Bool(true)], "03");

    assert_key_golden(&[Value::Int(0)], "048000000000000000");
    assert_key_golden(&[Value::Int(42)], "04800000000000002a");
    assert_key_golden(&[Value::Int(-1)], "047fffffffffffffff");
    assert_key_golden(&[Value::Int(i64::MIN)], "040000000000000000");
    assert_key_golden(&[Value::Int(i64::MAX)], "04ffffffffffffffff");

    assert_key_golden(&[Value::Float(0.0)], "058000000000000000");
    assert_key_golden(&[Value::Float(std::f64::consts::PI)], "05c00921fb54442d18");
    assert_key_golden(&[Value::Float(-std::f64::consts::PI)], "053ff6de04abbbd2e7");
    assert_key_golden(&[Value::Float(f64::NEG_INFINITY)], "05000fffffffffffff");
    assert_key_golden(&[Value::Float(f64::INFINITY)], "05fff0000000000000");

    assert_key_golden(&[Value::String(String::new())], "060000000000000000f7");
    assert_key_golden(&[Value::String("Host".into())], "06486f737400000000fb");
    assert_key_golden(&[Value::String("\u{6c34}".into())], "06e6b0b40000000000fa");
    assert_key_golden(
        &[Value::String("abcdefgh".into())],
        "066162636465666768ff0000000000000000f7",
    );

    assert_key_golden(&[Value::Bytes(vec![])], "070000000000000000f7");
    assert_key_golden(
        &[Value::Bytes((0..12).collect())],
        "070001020304050607ff08090a0b00000000fb",
    );
}

#[test]
fn golden_keys_temporal() {
    assert_key_golden(&[Value::Date(Date { days: 18993 })], "088000000000004a31");
    assert_key_golden(&[Value::Date(Date { days: -1 })], "087fffffffffffffff");
    assert_key_golden(
        &[Value::Time(Time {
            nanos: 43_200_000_000_000,
        })],
        "090000274a48a78000",
    );
    assert_key_golden(
        &[Value::DateTime(DateTime {
            epoch_nanos: 1_700_000_000_000_000_000,
            offset_minutes: 60,
        })],
        "0a97979cfe362a0000803c",
    );
    assert_key_golden(
        &[Value::Duration(Duration {
            months: -1,
            days: -2,
            nanos: -3,
        })],
        "0b7fffffffffffffff7ffffffffffffffe7ffffffffffffffd",
    );
}

#[test]
fn golden_keys_tuples_and_lists() {
    // A realistic node key: (label-ish string, int id).
    assert_key_golden(
        &[Value::String("Host".into()), Value::Int(42)],
        "06486f737400000000fb04800000000000002a",
    );
    assert_key_golden(&[Value::List(vec![])], "0c00");
    assert_key_golden(
        &[Value::List(vec![Value::Int(1), Value::String("a".into())])],
        "0c048000000000000001066100000000000000f800",
    );
    assert_key_golden(
        &[Value::List(vec![Value::List(vec![Value::Null])])],
        "0c0c010000",
    );
}

// ---------------------------------------------------------------------------
// Spec §3.3 map layouts (ADR-0008) — format_version 1 fixtures
// ---------------------------------------------------------------------------

fn host_web1() -> NodeKey {
    NodeKey::new("Host", vec![Value::String("web1".into())]).expect("valid")
}

fn service_db() -> NodeKey {
    NodeKey::new("Service", vec![Value::String("db".into())]).expect("valid")
}

#[test]
fn golden_node_keys() {
    // List element wrapping String("Host"), String("web1"):
    // 0c · string "Host" (one padded group, marker fb) · string "web1" ·
    // list end 00. Verified by hand against the pinned key primitives.
    let node = host_web1();
    let bytes = node.encode().expect("encode");
    assert_eq!(hex(&bytes), "0c06486f737400000000fb067765623100000000fb00");
    assert_eq!(NodeKey::decode(&bytes).expect("decode"), node);
    // The label prefix is the list opening plus the label element only.
    assert_eq!(hex(&node_label_prefix("Host")), "0c06486f737400000000fb");
}

#[test]
fn golden_edge_keys() {
    let edge = EdgeKey::new(host_web1(), "DEPENDS_ON", service_db(), Value::Null).expect("valid");
    // fwd = src node key ++ string "DEPENDS_ON" ++ dst node key ++ null.
    assert_eq!(
        hex(&edge.encode_fwd().expect("encode")),
        "0c06486f737400000000fb067765623100000000fb00\
         06444550454e44535fff4f4e000000000000f9\
         0c065365727669636500fe066462000000000000f900\
         01"
        .replace(char::is_whitespace, "")
    );
    // rev swaps the endpoints, nothing else.
    assert_eq!(
        hex(&edge.encode_rev().expect("encode")),
        "0c065365727669636500fe066462000000000000f900\
         06444550454e44535fff4f4e000000000000f9\
         0c06486f737400000000fb067765623100000000fb00\
         01"
        .replace(char::is_whitespace, "")
    );
}

#[test]
fn golden_index_entry() {
    let entry = IndexEntry::new(
        "Host",
        vec!["os".into()],
        vec![Value::String("linux".into())],
        host_web1(),
    )
    .expect("valid");
    // [label, [property names], [values], node key] — a single-property index
    // is one-element lists (composite indexes, ADR-0024 ratification/ADR-0027).
    assert_eq!(
        hex(&entry.encode().expect("encode")),
        "06486f737400000000fb0c066f73000000000000f9000c066c696e7578000000fc00\
         0c06486f737400000000fb067765623100000000fb00"
            .replace(char::is_whitespace, "")
    );
}

#[test]
fn golden_node_and_edge_records() {
    // [["Edge"], {"os": "linux", "cores": 8}] — property keys in canonical
    // (length-first) order: "os" before "cores".
    let record = NodeRecord::new(
        ["Edge".to_owned()],
        [
            ("os".to_owned(), Value::String("linux".into())),
            ("cores".to_owned(), Value::Int(8)),
        ]
        .into(),
    );
    let bytes = record.encode().expect("encode");
    assert_eq!(
        hex(&bytes),
        "82816445646765a2626f73656c696e757865636f72657308"
    );
    assert_eq!(NodeRecord::decode(&bytes).expect("decode"), record);

    // {"weight": 0.5} — bare properties map, shortest-form float.
    let edge = EdgeRecord::new([("weight".to_owned(), Value::Float(0.5))].into());
    let bytes = edge.encode().expect("encode");
    assert_eq!(hex(&bytes), "a166776569676874f93800");
    assert_eq!(EdgeRecord::decode(&bytes).expect("decode"), edge);
}

#[test]
fn golden_schema_entries() {
    let label = SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(
            vec!["name".into()],
            [("os".to_owned(), PropertyType::String)].into(),
            ["os".to_owned()],
            ["serial".to_owned()],
        )
        .expect("valid"),
    };
    assert_eq!(
        hex(&label.map_key()),
        "066c6162656c000000fc06486f737400000000fb"
    );
    // {"key": ["name"], "types": {"os": "string"}, "exists": ["os"],
    //  "unique": ["serial"], "surrogate": false} in canonical field order.
    assert_eq!(
        hex(&label.encode_value()),
        "a5636b657981646e616d65657479706573a1626f7366737472696e67\
         6665786973747381626f7366756e69717565816673657269616c\
         69737572726f67617465f4"
            .replace(char::is_whitespace, "")
    );

    let rtype = SchemaEntry::RelType {
        name: "DEPENDS_ON".into(),
        def: RelTypeDef::new(Some("port".into()), BTreeMap::new(), []).expect("valid"),
    };
    assert_eq!(
        hex(&rtype.map_key()),
        "067274797065000000fc06444550454e44535fff4f4e000000000000f9"
    );
    // {"disc": "port", "types": {}, "exists": []}.
    assert_eq!(
        hex(&rtype.encode_value()),
        "a3646469736364706f7274657479706573a06665786973747380"
    );

    let index = SchemaEntry::Index {
        name: "host_os".into(),
        def: IndexDef::new("Host", vec!["os".into()]).expect("valid"),
    };
    assert_eq!(
        hex(&index.map_key()),
        "06696e646578000000fc06686f73745f6f7300fe"
    );
    // {"label": "Host", "properties": ["os"]}.
    assert_eq!(
        hex(&index.encode_value()),
        "a2656c6162656c64486f73746a70726f7065727469657381626f73"
    );

    assert_eq!(hex(&schema_kind_prefix("label")), "066c6162656c000000fc");

    // All three decode back exactly.
    for entry in [label, rtype, index] {
        assert_eq!(
            SchemaEntry::decode(&entry.map_key(), &entry.encode_value()).expect("decode"),
            entry
        );
    }
}

#[test]
fn golden_manifest() {
    let root = |seed: u8, height: u32| MapRoot {
        hash: Hash::from_bytes(&[seed; 20]).expect("SHA-1 width"),
        height,
    };
    let manifest = Manifest {
        chunk_params: ChunkParams::new(1024, 12, 65536).expect("valid"),
        schema: root(0x11, 1),
        nodes: root(0x22, 2),
        edges_fwd: root(0x33, 1),
        edges_rev: root(0x44, 1),
        indexes: [("host_os".to_owned(), root(0x55, 1))].into(),
        conflicts: None,
    };
    // [1, {"nodes": …, "schema": …, "indexes": {…}, "edges_fwd": …,
    //      "edges_rev": …, "chunk_params": [1024, 12, 65536]}]
    let expected = "8201a6\
        656e6f6465738254222222222222222222222222222222222222222202\
        66736368656d618254111111111111111111111111111111111111111101\
        67696e6465786573a167686f73745f6f738254555555555555555555555555555555555555555501\
        6965646765735f667764825433333333333333333333333333333333333333330\
        16965646765735f726576825444444444444444444444444444444444444444440\
        16c6368756e6b5f706172616d73831904000c1a00010000"
        .replace(char::is_whitespace, "");
    let bytes = manifest.encode();
    assert_eq!(
        hex(&bytes),
        expected,
        "manifest encoding changed — this is a format_version bump"
    );
    assert_eq!(Manifest::decode(&bytes).expect("decode"), manifest);

    // Mid-merge variant carries a "conflicts" root between "indexes" and
    // "edges_fwd" (canonical key order).
    let merging = Manifest {
        conflicts: Some(root(0x66, 1)),
        ..manifest
    };
    let expected_merging = "8201a7\
        656e6f6465738254222222222222222222222222222222222222222202\
        66736368656d618254111111111111111111111111111111111111111101\
        67696e6465786573a167686f73745f6f738254555555555555555555555555555555555555555501\
        69636f6e666c69637473825466666666666666666666666666666666666666660\
        16965646765735f667764825433333333333333333333333333333333333333330\
        16965646765735f726576825444444444444444444444444444444444444444440\
        16c6368756e6b5f706172616d73831904000c1a00010000"
        .replace(char::is_whitespace, "");
    let bytes = merging.encode();
    assert_eq!(hex(&bytes), expected_merging);
    assert_eq!(Manifest::decode(&bytes).expect("decode"), merging);
}
