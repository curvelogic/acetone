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

use acetone_model::{Date, DateTime, Duration, Time, Value};
use acetone_model::{keys, values};

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
        "d910041b0000274a48a78000",
    );
    assert_value_golden(Value::Time(Time { nanos: 0 }), "d9100400");
    // 2023-11-14T22:13:20Z as 1_700_000_000_000_000_000 ns, offset +01:00.
    assert_value_golden(
        Value::DateTime(DateTime {
            epoch_nanos: 1_700_000_000_000_000_000,
            offset_minutes: 60,
        }),
        "d91005821b17979cfe362a0000183c",
    );
    assert_value_golden(
        Value::DateTime(DateTime {
            epoch_nanos: 0,
            offset_minutes: 0,
        }),
        "d91005820000",
    );
    assert_value_golden(
        Value::Duration(Duration {
            months: 1,
            days: 2,
            nanos: 3,
        }),
        "d9100683010203",
    );
    assert_value_golden(
        Value::Duration(Duration {
            months: -1,
            days: -2,
            nanos: -3,
        }),
        "d9100683202122",
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
