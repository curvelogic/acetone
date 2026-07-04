//! Property tests for the normative spec §3.4 encodings (Load-Bearing
//! Invariant 2): round trips, byte-order == logical-order for keys, strict
//! canonicity of decoding, and decoder totality on arbitrary bytes.

use acetone_model::{Date, DateTime, Duration, MAX_OFFSET_MINUTES, NANOS_PER_DAY, Time, Value};
use acetone_model::{keys, values};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/// Any float, including NaN, infinities, subnormals and -0.0.
fn any_float() -> impl Strategy<Value = f64> {
    prop_oneof![
        8 => any::<f64>(),
        1 => Just(f64::NAN),
        1 => Just(-f64::NAN),
        1 => Just(f64::from_bits(0x7ff8_dead_beef_0001)), // NaN payload
        1 => Just(f64::INFINITY),
        1 => Just(f64::NEG_INFINITY),
        1 => Just(0.0),
        1 => Just(-0.0),
        1 => Just(5e-324),  // smallest subnormal
        1 => Just(65504.0), // f16 max
        1 => Just(f64::MAX),
    ]
}

/// Floats valid in key positions: no NaN; -0.0 pre-normalised to +0.0 so
/// round trips compare bit-exactly.
fn key_float() -> impl Strategy<Value = f64> {
    any_float().prop_map(|x| {
        if x.is_nan() {
            0.5
        } else if x == 0.0 {
            0.0
        } else {
            x
        }
    })
}

fn any_int() -> impl Strategy<Value = i64> {
    prop_oneof![
        8 => any::<i64>(),
        1 => Just(i64::MIN),
        1 => Just(i64::MAX),
        1 => Just(0),
        1 => Just(-1),
        // Head-width boundaries in CBOR.
        1 => Just(23),
        1 => Just(24),
        1 => Just(255),
        1 => Just(256),
        1 => Just(65535),
        1 => Just(65536),
        1 => Just(4294967295),
        1 => Just(4294967296),
    ]
}

fn any_string() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => ".{0,24}",
        // Exercise the 8-byte chunk boundaries of the key framing.
        1 => proptest::collection::vec(any::<char>(), 7..=9)
            .prop_map(|cs| cs.into_iter().collect()),
        1 => Just(String::new()),
        1 => Just("\0\0\0\0\0\0\0\0".to_owned()),
    ]
}

fn any_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        6 => proptest::collection::vec(any::<u8>(), 0..32),
        1 => proptest::collection::vec(any::<u8>(), 7..=9),
        1 => Just(vec![]),
        1 => Just(vec![0u8; 8]),
        1 => Just(vec![0xff; 16]),
    ]
}

fn temporal_scalars() -> Vec<BoxedStrategy<Value>> {
    vec![
        any_int()
            .prop_map(|d| Value::Date(Date { days: d }))
            .boxed(),
        (0..NANOS_PER_DAY)
            .prop_map(|n| Value::Time(Time { nanos: n }))
            .boxed(),
        (any_int(), -MAX_OFFSET_MINUTES..=MAX_OFFSET_MINUTES)
            .prop_map(|(epoch_nanos, offset_minutes)| {
                Value::DateTime(DateTime {
                    epoch_nanos,
                    offset_minutes,
                })
            })
            .boxed(),
        (any_int(), any_int(), any_int())
            .prop_map(|(months, days, nanos)| {
                Value::Duration(Duration {
                    months,
                    days,
                    nanos,
                })
            })
            .boxed(),
    ]
}

fn scalar(float: fn() -> BoxedStrategy<f64>) -> impl Strategy<Value = Value> {
    let mut options = vec![
        Just(Value::Null).boxed(),
        any::<bool>().prop_map(Value::Bool).boxed(),
        any_int().prop_map(Value::Int).boxed(),
        float().prop_map(Value::Float).boxed(),
        any_string().prop_map(Value::String).boxed(),
        any_bytes().prop_map(Value::Bytes).boxed(),
    ];
    options.extend(temporal_scalars());
    proptest::strategy::Union::new(options)
}

fn value_strategy(float: fn() -> BoxedStrategy<f64>) -> impl Strategy<Value = Value> {
    scalar(float).prop_recursive(4, 48, 6, |inner| {
        proptest::collection::vec(inner, 0..6)
            .prop_map(Value::List)
            .boxed()
    })
}

/// Any value (floats may be NaN): valid for the values encoding.
fn any_value() -> impl Strategy<Value = Value> {
    value_strategy(|| any_float().boxed())
}

/// Values valid in key positions (no NaN, no -0.0).
fn key_value() -> impl Strategy<Value = Value> {
    value_strategy(|| key_float().boxed())
}

fn key_tuple() -> impl Strategy<Value = Vec<Value>> {
    proptest::collection::vec(key_value(), 0..4)
}

// ---------------------------------------------------------------------------
// Equality modulo NaN canonicalisation
// ---------------------------------------------------------------------------

/// Bit-exact equality except that any NaN equals any NaN (the values
/// encoder collapses NaN payloads by design).
fn canonical_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => {
            (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits()
        }
        (Value::List(xs), Value::List(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(x, y)| canonical_eq(x, y))
        }
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    /// Values: decode(encode(v)) == v (modulo NaN payload collapse) and the
    /// re-encoding is byte-identical (encoding is a function of the value).
    #[test]
    fn value_round_trip(v in any_value()) {
        let bytes = values::encode_value(&v).expect("encodable");
        let back = values::decode_value(&bytes).expect("decodable");
        prop_assert!(canonical_eq(&v, &back), "{v:?} != {back:?}");
        prop_assert_eq!(values::encode_value(&back).unwrap(), bytes);
    }

    /// Values: any bytes that decode successfully are exactly the canonical
    /// encoding of the decoded value (strict canonicity).
    #[test]
    fn value_decoding_accepts_only_canonical_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        if let Ok(v) = values::decode_value(&bytes) {
            prop_assert_eq!(values::encode_value(&v).unwrap(), bytes);
        }
    }

    /// Values: mutating one byte of a valid encoding either fails to decode
    /// or decodes to a different value — never silently the same value
    /// (each value has exactly one accepted byte form).
    #[test]
    fn value_encoding_is_injective_under_mutation(v in any_value(), idx in any::<prop::sample::Index>(), bit in 0u8..8) {
        let bytes = values::encode_value(&v).expect("encodable");
        prop_assume!(!bytes.is_empty());
        let mut mutated = bytes.clone();
        let i = idx.index(mutated.len());
        mutated[i] ^= 1 << bit;
        if let Ok(back) = values::decode_value(&mutated) {
            prop_assert!(!canonical_eq(&v, &back),
                "mutated bytes decoded to the same value {v:?}");
        }
    }

    /// Keys: decode(encode(t)) == t and re-encoding is byte-identical.
    #[test]
    fn key_round_trip(t in key_tuple()) {
        let bytes = keys::encode_key(&t).expect("encodable");
        let back = keys::decode_key(&bytes).expect("decodable");
        prop_assert_eq!(&back, &t);
        prop_assert_eq!(keys::encode_key(&back).unwrap(), bytes);
    }

    /// Keys: THE invariant — byte order of encodings equals logical tuple
    /// order under `key_cmp`.
    #[test]
    fn key_byte_order_is_logical_order(a in key_tuple(), b in key_tuple()) {
        let ea = keys::encode_key(&a).expect("encodable");
        let eb = keys::encode_key(&b).expect("encodable");
        prop_assert_eq!(
            ea.cmp(&eb),
            keys::key_cmp(&a, &b),
            "byte order disagrees with logical order for {:?} vs {:?}",
            a,
            b
        );
    }

    /// Keys: equal encodings iff logically equal (identity is injective).
    #[test]
    fn key_encoding_is_injective(a in key_tuple(), b in key_tuple()) {
        let ea = keys::encode_key(&a).expect("encodable");
        let eb = keys::encode_key(&b).expect("encodable");
        prop_assert_eq!(ea == eb, keys::key_cmp(&a, &b) == std::cmp::Ordering::Equal);
    }

    /// Keys: a tuple extended with more elements encodes to a byte string
    /// with the original as prefix (prefix scans == range scans).
    #[test]
    fn key_tuple_prefix_is_byte_prefix(a in key_tuple(), b in key_tuple()) {
        let ea = keys::encode_key(&a).expect("encodable");
        let mut ab = a.clone();
        ab.extend(b);
        let eab = keys::encode_key(&ab).expect("encodable");
        prop_assert!(eab.starts_with(&ea));
    }

    /// Keys: any bytes that decode successfully are exactly the canonical
    /// encoding of the decoded tuple.
    #[test]
    fn key_decoding_accepts_only_canonical_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        if let Ok(t) = keys::decode_key(&bytes) {
            prop_assert_eq!(keys::encode_key(&t).unwrap(), bytes);
        }
    }

    /// Both decoders are total on arbitrary input: no panics, no hangs.
    #[test]
    fn decoders_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = values::decode_value(&bytes);
        let _ = keys::decode_key(&bytes);
    }

    /// Both decoders survive hostile declared lengths: a truncated prefix of
    /// a valid encoding never panics or over-allocates.
    #[test]
    fn decoders_survive_truncation(v in any_value(), cut in any::<prop::sample::Index>()) {
        let bytes = values::encode_value(&v).expect("encodable");
        let n = cut.index(bytes.len() + 1);
        let _ = values::decode_value(&bytes[..n]);
        if let Ok(t) = keys::decode_key(&bytes[..n]) {
            // Coincidental success must still be canonical.
            prop_assert_eq!(keys::encode_key(&t).unwrap(), bytes[..n].to_vec());
        }
    }
}
