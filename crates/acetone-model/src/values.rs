//! Canonical deterministic CBOR value encoding (spec §3.4).
//!
//! This module encodes [`Value`]s as CBOR (RFC 8949) restricted to the
//! **core deterministic encoding requirements** of RFC 8949 §4.2.1:
//!
//! - all lengths are definite (indefinite-length items are rejected);
//! - integer heads use the shortest form that holds the value;
//! - floats use the shortest of float16/float32/float64 that represents
//!   the value exactly (preferred serialisation);
//! - map keys, when maps appear in later record layers, must be sorted
//!   byte-wise (no map values exist at this layer — spec §2 excludes
//!   nested maps from v0.1, so [`Value`] has no map variant).
//!
//! The decoder is **strict**: it accepts exactly the canonical form and
//! nothing else, so `decode(bytes)` succeeding implies
//! `encode(decode(bytes)) == bytes`. Wrong-but-well-formed CBOR (longer
//! integer heads, non-shortest floats, indefinite lengths, unknown tags,
//! trailing bytes) yields an error, never a silently re-canonicalised
//! value. The decoder never panics on untrusted input, allocates no more
//! than the input length implies, and enforces a nesting depth limit.
//!
//! # Type mapping
//!
//! | [`Value`]    | CBOR encoding                                          |
//! |--------------|--------------------------------------------------------|
//! | `Null`       | `f6` (major 7, simple 22)                              |
//! | `Bool`       | `f4` / `f5` (simple 20/21)                             |
//! | `Int`        | major 0 (≥ 0) or major 1 (< 0), shortest head          |
//! | `Float`      | major 7, shortest exact float16/float32/float64        |
//! | `String`     | major 3, definite length, UTF-8                        |
//! | `Bytes`      | major 2, definite length                               |
//! | `Date`       | tag 100 (RFC 8943), integer days since 1970-01-01      |
//! | `Time`       | tag 74100, unsigned integer nanoseconds since midnight |
//! | `DateTime`   | tag 74101, array `[epoch_nanos, offset_minutes]`       |
//! | `Duration`   | tag 74102, array `[months, days, nanos]`               |
//! | `List`       | major 4, definite length                               |
//!
//! Tags 74100–74102 are acetone-assigned from the CBOR first-come
//! first-served tag range (values of 32768 and up, RFC 8949 §9.2); they
//! are format-internal and not IANA-registered.
//!
//! # Float canonicalisation (format decisions)
//!
//! - **NaN payloads are not preserved.** Every NaN encodes as the
//!   canonical half-width quiet NaN `f9 7e00` (RFC 8949 §4.2.2), and
//!   decodes to the quiet NaN `0x7ff8_0000_0000_0000`. Two NaNs with
//!   different payloads are therefore identical on disk — required for
//!   deterministic hashing.
//! - **Negative zero is preserved** in values (`f9 8000`), unlike in key
//!   positions (see [`crate::keys`]).
//!
//! Any change to this encoding is a `format_version` bump (spec §10,
//! Load-Bearing Invariant 2).

use crate::cbor::{
    HEAD_F16, HEAD_F32, HEAD_F64, MAJOR_ARRAY, MAJOR_BYTES, MAJOR_MAP, MAJOR_NEGATIVE,
    MAJOR_SIMPLE, MAJOR_TAG, MAJOR_TEXT, MAJOR_UNSIGNED, Reader, SIMPLE_FALSE, SIMPLE_NULL,
    SIMPLE_TRUE, f16_bits_to_f64, f16_from_f64_exact, f32_from_f64_exact, negative_to_i64,
    read_i64, unsigned_to_i64, write_head, write_int,
};
use crate::{Date, DateTime, Duration, MAX_OFFSET_MINUTES, NANOS_PER_DAY, Time, Value};
use thiserror::Error;

/// CBOR tag for [`Date`]: days since the epoch date (RFC 8943).
pub const TAG_DATE: u64 = 100;
/// CBOR tag for [`Time`]: unsigned nanoseconds since midnight (acetone).
pub const TAG_TIME: u64 = 74100;
/// CBOR tag for [`DateTime`]: `[epoch_nanos, offset_minutes]` (acetone).
pub const TAG_DATETIME: u64 = 74101;
/// CBOR tag for [`Duration`]: `[months, days, nanos]` (acetone).
pub const TAG_DURATION: u64 = 74102;

/// Maximum nesting depth accepted by encoder and decoder.
pub const MAX_DEPTH: usize = 128;

/// The single canonical NaN bit pattern produced by decoding `f9 7e00`.
pub const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

/// Errors from [`encode_value`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueEncodeError {
    /// A [`Time`] was out of the valid range `0..NANOS_PER_DAY`.
    #[error("time of day out of range: {0} ns (must be < {NANOS_PER_DAY})")]
    TimeOutOfRange(u64),
    /// A [`DateTime`] offset was outside `±MAX_OFFSET_MINUTES`.
    #[error("UTC offset out of range: {0} minutes (must be within ±{MAX_OFFSET_MINUTES})")]
    OffsetOutOfRange(i16),
    /// Lists nested deeper than [`MAX_DEPTH`].
    #[error("value nesting exceeds maximum depth {MAX_DEPTH}")]
    DepthExceeded,
}

/// Errors from [`decode_value`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueDecodeError {
    /// Input ended before the value was complete.
    #[error("unexpected end of input")]
    UnexpectedEnd,
    /// Bytes remained after a complete value.
    #[error("trailing bytes after value")]
    TrailingBytes,
    /// A well-formed CBOR construct acetone does not use (maps, indefinite
    /// lengths, simple values other than false/true/null, bignums, ...).
    #[error("unsupported CBOR item: {0}")]
    Unsupported(&'static str),
    /// Well-formed CBOR that is not in canonical form.
    #[error("non-canonical encoding: {0}")]
    NonCanonical(&'static str),
    /// A malformed CBOR head (reserved additional-information values).
    #[error("malformed CBOR head")]
    MalformedHead,
    /// An integer outside the i64 range (acetone integers are i64).
    #[error("integer out of i64 range")]
    IntOutOfRange,
    /// A declared length exceeds the remaining input.
    #[error("declared length {declared} exceeds remaining input {remaining}")]
    LengthOverrun { declared: u64, remaining: usize },
    /// A text string was not valid UTF-8.
    #[error("invalid UTF-8 in string")]
    InvalidUtf8,
    /// A tag acetone does not recognise.
    #[error("unknown tag {0}")]
    UnknownTag(u64),
    /// Tag content had the wrong shape (e.g. non-integer date).
    #[error("invalid content for tag {tag}: {reason}")]
    InvalidTagContent { tag: u64, reason: &'static str },
    /// A decoded [`Time`] was out of range.
    #[error("time of day out of range")]
    TimeOutOfRange,
    /// A decoded [`DateTime`] offset was out of range.
    #[error("UTC offset out of range")]
    OffsetOutOfRange,
    /// Nesting deeper than [`MAX_DEPTH`].
    #[error("value nesting exceeds maximum depth {MAX_DEPTH}")]
    DepthExceeded,
}

/// Encode a value in canonical deterministic CBOR.
pub fn encode_value(value: &Value) -> Result<Vec<u8>, ValueEncodeError> {
    let mut out = Vec::new();
    encode_value_into(&mut out, value)?;
    Ok(out)
}

/// Encode a value in canonical deterministic CBOR, appending to `out`.
pub fn encode_value_into(out: &mut Vec<u8>, value: &Value) -> Result<(), ValueEncodeError> {
    write_value(out, value, 0)
}

/// Decode a canonical CBOR value, consuming the whole input.
///
/// Strict: succeeds only on exactly the bytes [`encode_value`] would
/// produce for the returned value.
pub fn decode_value(bytes: &[u8]) -> Result<Value, ValueDecodeError> {
    let mut reader = Reader {
        input: bytes,
        pos: 0,
    };
    let value = read_value(&mut reader, 0)?;
    if reader.remaining() != 0 {
        return Err(ValueDecodeError::TrailingBytes);
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Canonical float item, shortest exact form, NaN canonicalised. Lives
/// here rather than in [`crate::cbor`] because the NaN policy is a value-
/// layer format decision (see module docs).
fn write_float(out: &mut Vec<u8>, x: f64) {
    if x.is_nan() {
        // Canonical NaN: payloads are deliberately not preserved.
        out.push(HEAD_F16);
        out.extend_from_slice(&0x7e00_u16.to_be_bytes());
    } else if let Some(h) = f16_from_f64_exact(x) {
        out.push(HEAD_F16);
        out.extend_from_slice(&h.to_be_bytes());
    } else if let Some(s) = f32_from_f64_exact(x) {
        out.push(HEAD_F32);
        out.extend_from_slice(&s.to_bits().to_be_bytes());
    } else {
        out.push(HEAD_F64);
        out.extend_from_slice(&x.to_bits().to_be_bytes());
    }
}

pub(crate) fn write_value(
    out: &mut Vec<u8>,
    value: &Value,
    depth: usize,
) -> Result<(), ValueEncodeError> {
    if depth > MAX_DEPTH {
        return Err(ValueEncodeError::DepthExceeded);
    }
    match value {
        Value::Null => out.push(SIMPLE_NULL),
        Value::Bool(false) => out.push(SIMPLE_FALSE),
        Value::Bool(true) => out.push(SIMPLE_TRUE),
        Value::Int(n) => write_int(out, *n),
        Value::Float(x) => write_float(out, *x),
        Value::String(s) => {
            write_head(out, MAJOR_TEXT, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            write_head(out, MAJOR_BYTES, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Date(Date { days }) => {
            write_head(out, MAJOR_TAG, TAG_DATE);
            write_int(out, *days);
        }
        Value::Time(Time { nanos }) => {
            if *nanos >= NANOS_PER_DAY {
                return Err(ValueEncodeError::TimeOutOfRange(*nanos));
            }
            write_head(out, MAJOR_TAG, TAG_TIME);
            write_head(out, MAJOR_UNSIGNED, *nanos);
        }
        Value::DateTime(DateTime {
            epoch_nanos,
            offset_minutes,
        }) => {
            if offset_minutes.unsigned_abs() > MAX_OFFSET_MINUTES.unsigned_abs() {
                return Err(ValueEncodeError::OffsetOutOfRange(*offset_minutes));
            }
            write_head(out, MAJOR_TAG, TAG_DATETIME);
            write_head(out, MAJOR_ARRAY, 2);
            write_int(out, *epoch_nanos);
            write_int(out, i64::from(*offset_minutes));
        }
        Value::Duration(Duration {
            months,
            days,
            nanos,
        }) => {
            write_head(out, MAJOR_TAG, TAG_DURATION);
            write_head(out, MAJOR_ARRAY, 3);
            write_int(out, *months);
            write_int(out, *days);
            write_int(out, *nanos);
        }
        Value::List(items) => {
            write_head(out, MAJOR_ARRAY, items.len() as u64);
            for item in items {
                write_value(out, item, depth + 1)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

pub(crate) fn read_value(reader: &mut Reader, depth: usize) -> Result<Value, ValueDecodeError> {
    if depth > MAX_DEPTH {
        return Err(ValueDecodeError::DepthExceeded);
    }
    let ib = reader.read_u8()?;
    let major = ib >> 5;
    let ai = ib & 0x1f;
    match major {
        MAJOR_UNSIGNED => Ok(Value::Int(unsigned_to_i64(reader.read_head_value(ai)?)?)),
        MAJOR_NEGATIVE => Ok(Value::Int(negative_to_i64(reader.read_head_value(ai)?)?)),
        MAJOR_BYTES => {
            let len = reader.read_head_value(ai)?;
            let len = reader.check_len(len)?;
            Ok(Value::Bytes(reader.read_exact(len)?.to_vec()))
        }
        MAJOR_TEXT => {
            let len = reader.read_head_value(ai)?;
            let len = reader.check_len(len)?;
            let bytes = reader.read_exact(len)?;
            let s = str::from_utf8(bytes).map_err(|_| ValueDecodeError::InvalidUtf8)?;
            Ok(Value::String(s.to_owned()))
        }
        MAJOR_ARRAY => {
            let count = reader.read_head_value(ai)?;
            // Every element takes at least one byte; cap the allocation by
            // what the remaining input could possibly hold.
            if count > reader.remaining() as u64 {
                return Err(ValueDecodeError::LengthOverrun {
                    declared: count,
                    remaining: reader.remaining(),
                });
            }
            // Bounded speculative reservation (acetone-8gp): trust the
            // declared count only up to MAX_PREALLOC_ITEMS; beyond that the
            // vector grows as elements actually decode.
            let mut items =
                Vec::with_capacity((count as usize).min(crate::cbor::MAX_PREALLOC_ITEMS));
            for _ in 0..count {
                items.push(read_value(reader, depth + 1)?);
            }
            Ok(Value::List(items))
        }
        MAJOR_MAP => Err(ValueDecodeError::Unsupported("map")),
        MAJOR_TAG => {
            let tag = reader.read_head_value(ai)?;
            read_tagged(reader, tag)
        }
        MAJOR_SIMPLE => read_simple_or_float(reader, ai),
        _ => unreachable!("major type is 3 bits"),
    }
}

fn read_tagged(reader: &mut Reader, tag: u64) -> Result<Value, ValueDecodeError> {
    match tag {
        TAG_DATE => {
            // Reclassify only the wrong-shape error; truncation and
            // canonicality errors keep their precise kind.
            let days = read_i64(reader).map_err(|e| match e {
                ValueDecodeError::Unsupported("expected integer") => {
                    ValueDecodeError::InvalidTagContent {
                        tag: TAG_DATE,
                        reason: "expected integer days",
                    }
                }
                other => other,
            })?;
            Ok(Value::Date(Date { days }))
        }
        TAG_TIME => {
            let ib = reader.read_u8()?;
            if ib >> 5 != MAJOR_UNSIGNED {
                return Err(ValueDecodeError::InvalidTagContent {
                    tag: TAG_TIME,
                    reason: "expected unsigned integer nanoseconds",
                });
            }
            let nanos = reader.read_head_value(ib & 0x1f)?;
            if nanos >= NANOS_PER_DAY {
                return Err(ValueDecodeError::TimeOutOfRange);
            }
            Ok(Value::Time(Time { nanos }))
        }
        TAG_DATETIME => {
            expect_array(reader, 2, TAG_DATETIME)?;
            let epoch_nanos = read_i64(reader)?;
            let offset = read_i64(reader)?;
            let offset_minutes =
                i16::try_from(offset).map_err(|_| ValueDecodeError::OffsetOutOfRange)?;
            if offset_minutes.unsigned_abs() > MAX_OFFSET_MINUTES.unsigned_abs() {
                return Err(ValueDecodeError::OffsetOutOfRange);
            }
            Ok(Value::DateTime(DateTime {
                epoch_nanos,
                offset_minutes,
            }))
        }
        TAG_DURATION => {
            expect_array(reader, 3, TAG_DURATION)?;
            let months = read_i64(reader)?;
            let days = read_i64(reader)?;
            let nanos = read_i64(reader)?;
            Ok(Value::Duration(Duration {
                months,
                days,
                nanos,
            }))
        }
        other => Err(ValueDecodeError::UnknownTag(other)),
    }
}

fn expect_array(reader: &mut Reader, count: u64, tag: u64) -> Result<(), ValueDecodeError> {
    // Reclassify only the wrong-major error; truncation and canonicality
    // errors keep their precise kind.
    let n = reader.read_head(MAJOR_ARRAY).map_err(|e| match e {
        ValueDecodeError::Unsupported("unexpected major type") => {
            ValueDecodeError::InvalidTagContent {
                tag,
                reason: "expected array",
            }
        }
        other => other,
    })?;
    if n != count {
        return Err(ValueDecodeError::InvalidTagContent {
            tag,
            reason: "wrong array length",
        });
    }
    Ok(())
}

fn read_simple_or_float(reader: &mut Reader, ai: u8) -> Result<Value, ValueDecodeError> {
    match ai {
        20 => Ok(Value::Bool(false)),
        21 => Ok(Value::Bool(true)),
        22 => Ok(Value::Null),
        23 => Err(ValueDecodeError::Unsupported("undefined")),
        0..=19 => Err(ValueDecodeError::Unsupported("simple value")),
        24 => Err(ValueDecodeError::Unsupported("simple value")),
        25 => {
            let bits = reader.read_be_u64(2)? as u16;
            let exp = (bits >> 10) & 0x1f;
            let frac = bits & 0x03ff;
            if exp == 0x1f && frac != 0 {
                // A NaN: only the canonical quiet NaN is accepted.
                if bits != 0x7e00 {
                    return Err(ValueDecodeError::NonCanonical("non-canonical NaN"));
                }
                return Ok(Value::Float(f64::from_bits(CANONICAL_NAN_BITS)));
            }
            Ok(Value::Float(f16_bits_to_f64(bits)))
        }
        26 => {
            let bits = reader.read_be_u64(4)? as u32;
            let x = f32::from_bits(bits);
            if x.is_nan() {
                return Err(ValueDecodeError::NonCanonical("non-canonical NaN"));
            }
            let x = f64::from(x);
            if f16_from_f64_exact(x).is_some() {
                return Err(ValueDecodeError::NonCanonical("float not in shortest form"));
            }
            Ok(Value::Float(x))
        }
        27 => {
            let bits = reader.read_be_u64(8)?;
            let x = f64::from_bits(bits);
            if x.is_nan() {
                return Err(ValueDecodeError::NonCanonical("non-canonical NaN"));
            }
            if f16_from_f64_exact(x).is_some() || f32_from_f64_exact(x).is_some() {
                return Err(ValueDecodeError::NonCanonical("float not in shortest form"));
            }
            Ok(Value::Float(x))
        }
        28..=30 => Err(ValueDecodeError::MalformedHead),
        31 => Err(ValueDecodeError::Unsupported("break")),
        _ => unreachable!("additional information is 5 bits"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn enc(v: &Value) -> String {
        hex(&encode_value(v).expect("encode"))
    }

    fn roundtrip(v: Value) {
        let bytes = encode_value(&v).expect("encode");
        let back = decode_value(&bytes).expect("decode");
        assert_eq!(back, v, "round trip for {v:?}");
        // Canonical stability: re-encoding is byte-identical.
        assert_eq!(encode_value(&back).unwrap(), bytes);
    }

    // --- RFC 8949 Appendix A vectors -------------------------------------

    #[test]
    fn rfc8949_integer_vectors() {
        assert_eq!(enc(&Value::Int(0)), "00");
        assert_eq!(enc(&Value::Int(1)), "01");
        assert_eq!(enc(&Value::Int(10)), "0a");
        assert_eq!(enc(&Value::Int(23)), "17");
        assert_eq!(enc(&Value::Int(24)), "1818");
        assert_eq!(enc(&Value::Int(25)), "1819");
        assert_eq!(enc(&Value::Int(100)), "1864");
        assert_eq!(enc(&Value::Int(1000)), "1903e8");
        assert_eq!(enc(&Value::Int(1_000_000)), "1a000f4240");
        assert_eq!(enc(&Value::Int(1_000_000_000_000)), "1b000000e8d4a51000");
        assert_eq!(enc(&Value::Int(-1)), "20");
        assert_eq!(enc(&Value::Int(-10)), "29");
        assert_eq!(enc(&Value::Int(-100)), "3863");
        assert_eq!(enc(&Value::Int(-1000)), "3903e7");
        assert_eq!(enc(&Value::Int(i64::MAX)), "1b7fffffffffffffff");
        assert_eq!(enc(&Value::Int(i64::MIN)), "3b7fffffffffffffff");
    }

    #[test]
    fn rfc8949_float_vectors() {
        assert_eq!(enc(&Value::Float(0.0)), "f90000");
        assert_eq!(enc(&Value::Float(-0.0)), "f98000");
        assert_eq!(enc(&Value::Float(1.0)), "f93c00");
        assert_eq!(enc(&Value::Float(1.1)), "fb3ff199999999999a");
        assert_eq!(enc(&Value::Float(1.5)), "f93e00");
        assert_eq!(enc(&Value::Float(65504.0)), "f97bff");
        assert_eq!(enc(&Value::Float(100_000.0)), "fa47c35000");
        assert_eq!(enc(&Value::Float(3.402_823_466_385_288_6e38)), "fa7f7fffff");
        assert_eq!(enc(&Value::Float(1.0e300)), "fb7e37e43c8800759c");
        assert_eq!(enc(&Value::Float(5.960_464_477_539_063e-8)), "f90001");
        assert_eq!(enc(&Value::Float(0.000_061_035_156_25)), "f90400");
        assert_eq!(enc(&Value::Float(-4.0)), "f9c400");
        assert_eq!(enc(&Value::Float(-4.1)), "fbc010666666666666");
        assert_eq!(enc(&Value::Float(f64::INFINITY)), "f97c00");
        assert_eq!(enc(&Value::Float(f64::NEG_INFINITY)), "f9fc00");
        assert_eq!(enc(&Value::Float(f64::NAN)), "f97e00");
    }

    #[test]
    fn rfc8949_string_bytes_array_vectors() {
        assert_eq!(enc(&Value::Bool(false)), "f4");
        assert_eq!(enc(&Value::Bool(true)), "f5");
        assert_eq!(enc(&Value::Null), "f6");
        assert_eq!(enc(&Value::String(String::new())), "60");
        assert_eq!(enc(&Value::String("a".into())), "6161");
        assert_eq!(enc(&Value::String("IETF".into())), "6449455446");
        assert_eq!(enc(&Value::String("\u{fc}".into())), "62c3bc");
        assert_eq!(enc(&Value::String("\u{6c34}".into())), "63e6b0b4");
        assert_eq!(enc(&Value::Bytes(vec![])), "40");
        assert_eq!(enc(&Value::Bytes(vec![1, 2, 3, 4])), "4401020304");
        assert_eq!(enc(&Value::List(vec![])), "80");
        assert_eq!(
            enc(&Value::List(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3)
            ])),
            "83010203"
        );
    }

    /// Gate D freeze-audit nit (acetone-093): pin the canonical text- and
    /// byte-string head form at every length-regime boundary (23/24,
    /// 255/256, 65535/65536 bytes) and its strict round trip.
    #[test]
    fn string_head_length_boundaries_round_trip_with_pinned_heads() {
        let cases: &[(usize, &[u8], &[u8])] = &[
            // (length, expected text head, expected bytes head)
            (23, &[0x77], &[0x57]),
            (24, &[0x78, 24], &[0x58, 24]),
            (255, &[0x78, 255], &[0x58, 255]),
            (256, &[0x79, 0x01, 0x00], &[0x59, 0x01, 0x00]),
            (65_535, &[0x79, 0xff, 0xff], &[0x59, 0xff, 0xff]),
            (
                65_536,
                &[0x7a, 0x00, 0x01, 0x00, 0x00],
                &[0x5a, 0x00, 0x01, 0x00, 0x00],
            ),
        ];
        for (len, text_head, bytes_head) in cases {
            let s = Value::String("a".repeat(*len));
            let encoded = encode_value(&s).expect("encode");
            assert!(
                encoded.starts_with(text_head),
                "text head for length {len}: got {:0>2x?}",
                &encoded[..text_head.len().min(encoded.len())]
            );
            assert_eq!(encoded.len(), text_head.len() + len, "text length {len}");
            assert_eq!(decode_value(&encoded).expect("decode"), s);

            let b = Value::Bytes(vec![0xaa; *len]);
            let encoded = encode_value(&b).expect("encode");
            assert!(
                encoded.starts_with(bytes_head),
                "bytes head for length {len}: got {:0>2x?}",
                &encoded[..bytes_head.len().min(encoded.len())]
            );
            assert_eq!(encoded.len(), bytes_head.len() + len, "bytes length {len}");
            assert_eq!(decode_value(&encoded).expect("decode"), b);
        }
    }

    // --- NaN canonicalisation ---------------------------------------------

    #[test]
    fn nan_payloads_collapse_to_canonical_nan() {
        for bits in [
            0x7ff8_0000_0000_0000_u64, // quiet NaN
            0x7ff8_0000_0000_0001,     // payload
            0x7ff0_0000_0000_0001,     // signalling
            0xfff8_0000_0000_0000,     // negative quiet NaN
            0xffff_ffff_ffff_ffff,     // all ones
        ] {
            let v = Value::Float(f64::from_bits(bits));
            let bytes = encode_value(&v).unwrap();
            assert_eq!(hex(&bytes), "f97e00");
            let back = decode_value(&bytes).unwrap();
            match back {
                Value::Float(x) => assert_eq!(x.to_bits(), CANONICAL_NAN_BITS),
                other => panic!("expected float, got {other:?}"),
            }
        }
    }

    // --- round trips --------------------------------------------------------

    #[test]
    fn round_trips() {
        roundtrip(Value::Null);
        roundtrip(Value::Bool(true));
        roundtrip(Value::Bool(false));
        for n in [0, 1, -1, 23, 24, -24, -25, i64::MAX, i64::MIN] {
            roundtrip(Value::Int(n));
        }
        for x in [
            0.0,
            -0.0,
            1.0,
            -1.5,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            f64::MAX,
            5e-324, // smallest subnormal
        ] {
            roundtrip(Value::Float(x));
        }
        roundtrip(Value::String(String::new()));
        roundtrip(Value::String("héllo — wörld".into()));
        roundtrip(Value::Bytes(vec![]));
        roundtrip(Value::Bytes((0..=255).collect()));
        roundtrip(Value::Date(Date { days: 0 }));
        roundtrip(Value::Date(Date { days: -719_468 }));
        roundtrip(Value::Time(Time { nanos: 0 }));
        roundtrip(Value::Time(Time {
            nanos: NANOS_PER_DAY - 1,
        }));
        roundtrip(Value::DateTime(DateTime {
            epoch_nanos: i64::MIN,
            offset_minutes: -1080,
        }));
        roundtrip(Value::DateTime(DateTime {
            epoch_nanos: 1_700_000_000_000_000_000,
            offset_minutes: 60,
        }));
        roundtrip(Value::Duration(Duration {
            months: -1,
            days: 40,
            nanos: 999,
        }));
        roundtrip(Value::List(vec![]));
        roundtrip(Value::List(vec![
            Value::Int(1),
            Value::List(vec![Value::String("nested".into())]),
            Value::Null,
        ]));
    }

    // --- encoder validation --------------------------------------------------

    #[test]
    fn encoder_rejects_out_of_range_temporal() {
        assert_eq!(
            encode_value(&Value::Time(Time {
                nanos: NANOS_PER_DAY
            })),
            Err(ValueEncodeError::TimeOutOfRange(NANOS_PER_DAY))
        );
        assert_eq!(
            encode_value(&Value::DateTime(DateTime {
                epoch_nanos: 0,
                offset_minutes: 1081
            })),
            Err(ValueEncodeError::OffsetOutOfRange(1081))
        );
        assert_eq!(
            encode_value(&Value::DateTime(DateTime {
                epoch_nanos: 0,
                offset_minutes: -1081
            })),
            Err(ValueEncodeError::OffsetOutOfRange(-1081))
        );
    }

    /// Regression: `i16::MIN` has no i16 absolute value, so a naive
    /// `.abs()` range check panics in debug builds and wraps (accepting
    /// the value) in release builds. Both directions must reject it
    /// identically in every build profile.
    #[test]
    fn offset_i16_min_is_rejected_without_panicking() {
        // Encode direction.
        assert_eq!(
            encode_value(&Value::DateTime(DateTime {
                epoch_nanos: 0,
                offset_minutes: i16::MIN
            })),
            Err(ValueEncodeError::OffsetOutOfRange(i16::MIN))
        );
        // Decode direction: [0, -32768] under the datetime tag.
        let mut bytes = Vec::new();
        write_head(&mut bytes, MAJOR_TAG, TAG_DATETIME);
        write_head(&mut bytes, MAJOR_ARRAY, 2);
        write_int(&mut bytes, 0);
        write_int(&mut bytes, -32768);
        assert_eq!(
            decode_value(&bytes),
            Err(ValueDecodeError::OffsetOutOfRange)
        );
    }

    #[test]
    fn encoder_rejects_excessive_depth() {
        let mut v = Value::Int(0);
        for _ in 0..(MAX_DEPTH + 2) {
            v = Value::List(vec![v]);
        }
        assert_eq!(encode_value(&v), Err(ValueEncodeError::DepthExceeded));
    }

    // --- strict decoding -----------------------------------------------------

    fn de(hex_str: &str) -> Result<Value, ValueDecodeError> {
        let bytes: Vec<u8> = (0..hex_str.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).unwrap())
            .collect();
        decode_value(&bytes)
    }

    #[test]
    fn decoder_rejects_overlong_heads() {
        assert_eq!(
            de("1817"),
            Err(ValueDecodeError::NonCanonical("overlong head"))
        );
        assert_eq!(
            de("190017"),
            Err(ValueDecodeError::NonCanonical("overlong head"))
        );
        assert_eq!(
            de("1a00000017"),
            Err(ValueDecodeError::NonCanonical("overlong head"))
        );
        assert_eq!(
            de("1b0000000000000017"),
            Err(ValueDecodeError::NonCanonical("overlong head"))
        );
        // Overlong array/string/bytes length heads too.
        assert_eq!(
            de("9800"),
            Err(ValueDecodeError::NonCanonical("overlong head"))
        );
        assert_eq!(
            de("7801ff"),
            Err(ValueDecodeError::NonCanonical("overlong head"))
        );
    }

    #[test]
    fn decoder_rejects_non_shortest_floats() {
        // 1.0 as f64 and f32 (canonical is f9 3c00).
        assert_eq!(
            de("fb3ff0000000000000"),
            Err(ValueDecodeError::NonCanonical("float not in shortest form"))
        );
        assert_eq!(
            de("fa3f800000"),
            Err(ValueDecodeError::NonCanonical("float not in shortest form"))
        );
        // 100000.0 as f64 (canonical is fa 47c35000).
        assert_eq!(
            de("fb40f86a0000000000"),
            Err(ValueDecodeError::NonCanonical("float not in shortest form"))
        );
    }

    #[test]
    fn decoder_rejects_non_canonical_nan() {
        assert_eq!(
            de("f97e01"),
            Err(ValueDecodeError::NonCanonical("non-canonical NaN"))
        );
        assert_eq!(
            de("f9fe00"),
            Err(ValueDecodeError::NonCanonical("non-canonical NaN"))
        );
        assert_eq!(
            de("fa7fc00000"),
            Err(ValueDecodeError::NonCanonical("non-canonical NaN"))
        );
        assert_eq!(
            de("fb7ff8000000000000"),
            Err(ValueDecodeError::NonCanonical("non-canonical NaN"))
        );
    }

    #[test]
    fn decoder_rejects_indefinite_and_unsupported() {
        assert_eq!(
            de("9f01ff"),
            Err(ValueDecodeError::Unsupported("indefinite length"))
        );
        assert_eq!(
            de("7f6161ff"),
            Err(ValueDecodeError::Unsupported("indefinite length"))
        );
        assert_eq!(
            de("5f4101ff"),
            Err(ValueDecodeError::Unsupported("indefinite length"))
        );
        assert_eq!(de("a0"), Err(ValueDecodeError::Unsupported("map")));
        assert_eq!(de("f7"), Err(ValueDecodeError::Unsupported("undefined")));
        assert_eq!(de("f0"), Err(ValueDecodeError::Unsupported("simple value")));
        assert_eq!(
            de("f820"),
            Err(ValueDecodeError::Unsupported("simple value"))
        );
        // Unknown tag (tag 0: standard datetime string, not an acetone tag).
        assert_eq!(de("c074"), Err(ValueDecodeError::UnknownTag(0)));
        // Bignums are unknown tags here.
        assert_eq!(
            de("c249010000000000000000"),
            Err(ValueDecodeError::UnknownTag(2))
        );
    }

    #[test]
    fn decoder_rejects_integers_out_of_i64_range() {
        // 2^64 - 1 (valid CBOR unsigned, too big for acetone Int).
        assert_eq!(
            de("1bffffffffffffffff"),
            Err(ValueDecodeError::IntOutOfRange)
        );
        // -2^64 (valid CBOR negative, too small).
        assert_eq!(
            de("3bffffffffffffffff"),
            Err(ValueDecodeError::IntOutOfRange)
        );
        // -(2^63) - 1: just below i64::MIN.
        assert_eq!(
            de("3b8000000000000000"),
            Err(ValueDecodeError::IntOutOfRange)
        );
    }

    #[test]
    fn decoder_rejects_truncation_and_trailing() {
        assert_eq!(de(""), Err(ValueDecodeError::UnexpectedEnd));
        assert_eq!(de("18"), Err(ValueDecodeError::UnexpectedEnd));
        assert_eq!(
            de("62c3"),
            Err(ValueDecodeError::LengthOverrun {
                declared: 2,
                remaining: 1
            })
        );
        // Array of three with one element present: the count precheck
        // fires before any element is read.
        assert_eq!(
            de("8301"),
            Err(ValueDecodeError::LengthOverrun {
                declared: 3,
                remaining: 1
            })
        );
        // Array of two whose first element consumes the remaining input
        // passes the count precheck but runs out on the second element.
        assert_eq!(de("821818"), Err(ValueDecodeError::UnexpectedEnd));
        assert_eq!(de("0000"), Err(ValueDecodeError::TrailingBytes));
        // Declared length far beyond input must not allocate or panic.
        assert_eq!(
            de("5b7fffffffffffffff"),
            Err(ValueDecodeError::LengthOverrun {
                declared: 0x7fff_ffff_ffff_ffff,
                remaining: 0
            })
        );
        assert_eq!(
            de("9b7fffffffffffffff"),
            Err(ValueDecodeError::LengthOverrun {
                declared: 0x7fff_ffff_ffff_ffff,
                remaining: 0
            })
        );
    }

    #[test]
    fn decoder_rejects_invalid_utf8() {
        assert_eq!(de("61ff"), Err(ValueDecodeError::InvalidUtf8));
    }

    #[test]
    fn decoder_rejects_bad_tag_content() {
        // Date with a string payload.
        assert!(matches!(
            de("d8646161"),
            Err(ValueDecodeError::InvalidTagContent { .. })
        ));
        // Time with negative nanos.
        assert!(matches!(
            de("da0001217420"),
            Err(ValueDecodeError::InvalidTagContent { .. })
        ));
        // Time out of range (86400e9 exactly).
        assert_eq!(
            de("da000121741b00004e94914f0000"),
            Err(ValueDecodeError::TimeOutOfRange)
        );
        // DateTime with wrong arity.
        assert!(matches!(
            de("da000121758100"),
            Err(ValueDecodeError::InvalidTagContent { .. })
        ));
        // DateTime with out-of-range offset (2000 minutes).
        assert_eq!(
            de("da000121758200 1907d0".replace(' ', "").as_str()),
            Err(ValueDecodeError::OffsetOutOfRange)
        );
        // Duration with wrong arity.
        assert!(matches!(
            de("da00012176820001"),
            Err(ValueDecodeError::InvalidTagContent { .. })
        ));
    }

    /// The pre-format-fix tags 4100–4102 sat in RFC 8949 §9.2's
    /// Specification Required range; the format now uses 74100–74102
    /// (first-come first-served range) and the old tags are unknown.
    #[test]
    fn decoder_rejects_retired_tags() {
        assert_eq!(de("d9100400"), Err(ValueDecodeError::UnknownTag(4100)));
        assert_eq!(de("d91005820000"), Err(ValueDecodeError::UnknownTag(4101)));
        assert_eq!(
            de("d9100683010203"),
            Err(ValueDecodeError::UnknownTag(4102))
        );
    }

    /// Tag-content errors are reclassified as `InvalidTagContent` only for
    /// wrong-shape content; truncation, canonicality and indefinite-length
    /// errors keep their precise kind.
    #[test]
    fn tag_content_errors_preserve_inner_kind() {
        // Truncated date content.
        assert_eq!(de("d864"), Err(ValueDecodeError::UnexpectedEnd));
        // Overlong head inside date content.
        assert_eq!(
            de("d8641817"),
            Err(ValueDecodeError::NonCanonical("overlong head"))
        );
        // Truncated datetime content (no array head at all).
        assert_eq!(de("da00012175"), Err(ValueDecodeError::UnexpectedEnd));
        // Indefinite-length array under the datetime tag.
        assert_eq!(
            de("da000121759f0000ff"),
            Err(ValueDecodeError::Unsupported("indefinite length"))
        );
    }

    #[test]
    fn decoder_enforces_depth_limit() {
        // 200 nested single-element arrays.
        let mut bytes = vec![0x81u8; 200];
        bytes.push(0x00);
        assert_eq!(decode_value(&bytes), Err(ValueDecodeError::DepthExceeded));
    }

    // --- f16 helpers ----------------------------------------------------------

    #[test]
    fn f16_conversion_is_exact_for_all_bit_patterns() {
        for bits in 0..=u16::MAX {
            let exp = (bits >> 10) & 0x1f;
            let frac = bits & 0x3ff;
            if exp == 0x1f && frac != 0 {
                continue; // NaN patterns handled separately
            }
            let x = f16_bits_to_f64(bits);
            assert_eq!(
                f16_from_f64_exact(x),
                Some(bits),
                "f16 bits {bits:#06x} did not round-trip (value {x})"
            );
        }
    }

    #[test]
    fn f16_rejects_inexact() {
        assert_eq!(f16_from_f64_exact(1.1), None);
        assert_eq!(f16_from_f64_exact(100_000.0), None);
        assert_eq!(f16_from_f64_exact(65505.0), None);
        assert_eq!(f16_from_f64_exact(1e-10), None);
        assert_eq!(f16_from_f64_exact(5e-324), None);
    }
}
