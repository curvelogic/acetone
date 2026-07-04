//! Order-preserving (memcomparable) key tuple encoding (spec §3.4).
//!
//! A key is an ordered tuple of [`Value`]s. Each element is encoded as a
//! single type-tag byte followed by a type-specific, self-delimiting body,
//! and the tuple encoding is the concatenation of its elements. The
//! defining property, enforced by property tests, is:
//!
//! > comparing encoded keys as byte strings equals comparing the logical
//! > tuples under [`key_cmp`] (byte order == logical order),
//!
//! so prolly-tree range scans equal label/prefix scans. A tuple that is a
//! prefix of another encodes to a byte-string prefix of the other, which
//! is exactly what prefix range scans need.
//!
//! # Type tags and cross-type order
//!
//! Values of different types order by type tag. The ordering is **stable
//! and documented, not semantically meaningful** (key columns are
//! scalar-typed per schema, so cross-type comparisons only arise in
//! mixed-type index maps and must merely be deterministic). In particular
//! `Int` and `Float` are distinct tags and do **not** interleave
//! numerically.
//!
//! | tag    | type          | body                                            |
//! |--------|---------------|-------------------------------------------------|
//! | `0x00` | *(list end)*  | terminator inside lists; never starts a value   |
//! | `0x01` | Null          | empty                                           |
//! | `0x02` | Bool false    | empty                                           |
//! | `0x03` | Bool true     | empty                                           |
//! | `0x04` | Int           | 8 bytes big-endian, sign bit flipped            |
//! | `0x05` | Float         | 8 bytes big-endian, IEEE-754 total-order form   |
//! | `0x06` | String        | chunked framing (below), UTF-8 bytes            |
//! | `0x07` | Bytes         | chunked framing (below)                         |
//! | `0x08` | Date          | 8 bytes: days since epoch, sign flipped         |
//! | `0x09` | Time          | 8 bytes big-endian: nanos since midnight        |
//! | `0x0a` | DateTime      | 8 bytes epoch nanos (sign flipped) + 2 bytes    |
//! |        |               | offset minutes (sign flipped)                   |
//! | `0x0b` | Duration      | 3 × 8 bytes sign flipped: months, days, nanos   |
//! | `0x0c` | List          | element encodings, then `0x00` terminator       |
//!
//! # Per-type transforms
//!
//! - **Integers** (and integer-shaped temporal components): the i64 is
//!   reinterpreted as u64 with the sign bit flipped and written big-endian,
//!   so unsigned byte order equals signed numeric order.
//! - **Floats**: the IEEE-754 total-order transform — if the sign bit is
//!   set, all 64 bits are inverted; otherwise the sign bit is set. Byte
//!   order then equals numeric order (−∞ … −0/+0 … +∞).
//! - **Strings/bytes** use chunked framing rather than NUL-escaping: the
//!   data is written in 8-byte groups, each followed by a marker byte.
//!   A full group with more data following gets marker `0xff`; the final
//!   group is zero-padded to 8 bytes and gets marker `0xf7 + n` where `n`
//!   is the number of meaningful bytes (0–8, so `0xf7..=0xfe` — a final
//!   *full* group is followed by an extra empty group with marker `0xf7`).
//!   This is order-preserving for arbitrary byte content, including
//!   embedded NULs, at a fixed 9/8 overhead, and every byte string has
//!   exactly one encoding. UTF-8 byte order equals Unicode code-point
//!   order, so string keys sort by code point.
//! - **Lists** encode their elements back to back and close with `0x00`,
//!   which is below every type tag, so a list that is a prefix of another
//!   sorts first, and list order is elementwise lexicographic.
//! - **DateTime** orders by instant first (`epoch_nanos`), then by offset
//!   as a tiebreak between distinct values denoting the same instant.
//! - **Duration** orders structurally by (months, days, nanos); this is
//!   not a temporal-length order (none exists — a month is not a fixed
//!   span). It is deterministic, which is all key order requires.
//!
//! # Format decisions (flagged for ADR)
//!
//! - **NaN is rejected in key positions** ([`KeyEncodeError::NanNotPermitted`]).
//!   NaN breaks the equality/identity semantics merge determinism relies
//!   on. Values (see [`crate::values`]) may carry NaN. Consequence: the
//!   index layer must decide how to handle NaN in indexed float properties
//!   (e.g. exclude such entries or reject them).
//! - **Negative zero is normalised to positive zero** at key-encode time,
//!   so `-0.0` and `0.0` — equal in openCypher — cannot become two
//!   distinct keys. Values preserve the sign of zero.
//!
//! The decoder is strict and total: it never panics on untrusted input,
//! rejects non-canonical padding/markers and encodings the encoder would
//! never produce (NaN or `-0.0` float bodies), bounds every allocation by
//! the input length, and enforces a nesting depth limit. Any change to
//! this encoding is a `format_version` bump (spec §10, Load-Bearing
//! Invariant 2).

use crate::{Date, DateTime, Duration, MAX_OFFSET_MINUTES, NANOS_PER_DAY, Time, Value};
use std::cmp::Ordering;
use thiserror::Error;

/// Maximum list nesting depth accepted by encoder and decoder.
pub const MAX_DEPTH: usize = 128;

const TAG_LIST_END: u8 = 0x00;
const TAG_NULL: u8 = 0x01;
const TAG_FALSE: u8 = 0x02;
const TAG_TRUE: u8 = 0x03;
const TAG_INT: u8 = 0x04;
const TAG_FLOAT: u8 = 0x05;
const TAG_STRING: u8 = 0x06;
const TAG_BYTES: u8 = 0x07;
const TAG_DATE: u8 = 0x08;
const TAG_TIME: u8 = 0x09;
const TAG_DATETIME: u8 = 0x0a;
const TAG_DURATION: u8 = 0x0b;
const TAG_LIST: u8 = 0x0c;

const SIGN_BIT: u64 = 1 << 63;
/// Marker for a final chunk group with zero meaningful bytes.
const MARKER_BASE: u8 = 0xf7;
/// Marker for a full group with more data following.
const MARKER_CONTINUE: u8 = 0xff;
const GROUP: usize = 8;

/// Errors from [`encode_key`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeyEncodeError {
    /// NaN floats are not permitted in key positions (format decision).
    #[error("NaN is not permitted in key positions")]
    NanNotPermitted,
    /// A [`Time`] was out of the valid range `0..NANOS_PER_DAY`.
    #[error("time of day out of range: {0} ns (must be < {NANOS_PER_DAY})")]
    TimeOutOfRange(u64),
    /// A [`DateTime`] offset was outside `±MAX_OFFSET_MINUTES`.
    #[error("UTC offset out of range: {0} minutes (must be within ±{MAX_OFFSET_MINUTES})")]
    OffsetOutOfRange(i16),
    /// Lists nested deeper than [`MAX_DEPTH`].
    #[error("key nesting exceeds maximum depth {MAX_DEPTH}")]
    DepthExceeded,
}

/// Errors from [`decode_key`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum KeyDecodeError {
    /// Input ended before the element was complete.
    #[error("unexpected end of input")]
    UnexpectedEnd,
    /// A byte that is not a valid type tag where a tag was expected.
    #[error("unknown type tag {0:#04x}")]
    UnknownTag(u8),
    /// A chunk marker byte outside `0xf7..=0xff`.
    #[error("invalid chunk marker {0:#04x}")]
    InvalidChunkMarker(u8),
    /// Well-formed but not the canonical encoding (non-zero padding, NaN
    /// or negative-zero float bodies, out-of-range temporal fields).
    #[error("non-canonical encoding: {0}")]
    NonCanonical(&'static str),
    /// A string key body was not valid UTF-8.
    #[error("invalid UTF-8 in string key")]
    InvalidUtf8,
    /// Nesting deeper than [`MAX_DEPTH`].
    #[error("key nesting exceeds maximum depth {MAX_DEPTH}")]
    DepthExceeded,
}

/// Encode a key tuple into its order-preserving byte form.
pub fn encode_key(tuple: &[Value]) -> Result<Vec<u8>, KeyEncodeError> {
    let mut out = Vec::new();
    encode_key_into(&mut out, tuple)?;
    Ok(out)
}

/// Encode a key tuple, appending to `out` (for composing prefixes).
pub fn encode_key_into(out: &mut Vec<u8>, tuple: &[Value]) -> Result<(), KeyEncodeError> {
    for value in tuple {
        write_element(out, value, 0)?;
    }
    Ok(())
}

/// Decode a key back into its tuple of values, consuming the whole input.
///
/// Strict: succeeds only on exactly the bytes [`encode_key`] would produce
/// for the returned tuple.
pub fn decode_key(bytes: &[u8]) -> Result<Vec<Value>, KeyDecodeError> {
    let mut reader = Reader {
        input: bytes,
        pos: 0,
    };
    let mut tuple = Vec::new();
    while reader.remaining() > 0 {
        tuple.push(read_element(&mut reader, 0)?);
    }
    Ok(tuple)
}

/// The logical total order on key tuples that the byte encoding realises:
/// `encode_key(a).cmp(&encode_key(b)) == key_cmp(a, b)` for all encodable
/// tuples (property-tested).
///
/// Elementwise lexicographic; elements of different types order by type
/// tag (see module docs). For floats, `-0.0` equals `0.0` (both encode
/// identically) and NaN — though not encodable — sorts after `+∞` via
/// IEEE total order, keeping this function total.
pub fn key_cmp(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = value_cmp(x, y);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => TAG_NULL,
        Value::Bool(false) => TAG_FALSE,
        Value::Bool(true) => TAG_TRUE,
        Value::Int(_) => TAG_INT,
        Value::Float(_) => TAG_FLOAT,
        Value::String(_) => TAG_STRING,
        Value::Bytes(_) => TAG_BYTES,
        Value::Date(_) => TAG_DATE,
        Value::Time(_) => TAG_TIME,
        Value::DateTime(_) => TAG_DATETIME,
        Value::Duration(_) => TAG_DURATION,
        Value::List(_) => TAG_LIST,
    }
}

fn value_cmp(a: &Value, b: &Value) -> Ordering {
    let by_rank = rank(a).cmp(&rank(b));
    if by_rank != Ordering::Equal {
        return by_rank;
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => normalise_zero(*x).total_cmp(&normalise_zero(*y)),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::Time(x), Value::Time(y)) => x.cmp(y),
        (Value::DateTime(x), Value::DateTime(y)) => x.cmp(y),
        (Value::Duration(x), Value::Duration(y)) => x.cmp(y),
        (Value::List(x), Value::List(y)) => key_cmp(x, y),
        _ => unreachable!("equal ranks imply equal variants"),
    }
}

fn normalise_zero(x: f64) -> f64 {
    if x == 0.0 { 0.0 } else { x }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn flip_sign_i64(n: i64) -> [u8; 8] {
    ((n as u64) ^ SIGN_BIT).to_be_bytes()
}

fn write_element(out: &mut Vec<u8>, value: &Value, depth: usize) -> Result<(), KeyEncodeError> {
    if depth > MAX_DEPTH {
        return Err(KeyEncodeError::DepthExceeded);
    }
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(false) => out.push(TAG_FALSE),
        Value::Bool(true) => out.push(TAG_TRUE),
        Value::Int(n) => {
            out.push(TAG_INT);
            out.extend_from_slice(&flip_sign_i64(*n));
        }
        Value::Float(x) => {
            if x.is_nan() {
                return Err(KeyEncodeError::NanNotPermitted);
            }
            out.push(TAG_FLOAT);
            let bits = normalise_zero(*x).to_bits();
            let enc = if bits & SIGN_BIT != 0 {
                !bits
            } else {
                bits | SIGN_BIT
            };
            out.extend_from_slice(&enc.to_be_bytes());
        }
        Value::String(s) => {
            out.push(TAG_STRING);
            write_chunked(out, s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(TAG_BYTES);
            write_chunked(out, b);
        }
        Value::Date(Date { days }) => {
            out.push(TAG_DATE);
            out.extend_from_slice(&flip_sign_i64(*days));
        }
        Value::Time(Time { nanos }) => {
            if *nanos >= NANOS_PER_DAY {
                return Err(KeyEncodeError::TimeOutOfRange(*nanos));
            }
            out.push(TAG_TIME);
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::DateTime(DateTime {
            epoch_nanos,
            offset_minutes,
        }) => {
            if offset_minutes.abs() > MAX_OFFSET_MINUTES {
                return Err(KeyEncodeError::OffsetOutOfRange(*offset_minutes));
            }
            out.push(TAG_DATETIME);
            out.extend_from_slice(&flip_sign_i64(*epoch_nanos));
            out.extend_from_slice(&((*offset_minutes as u16) ^ 0x8000).to_be_bytes());
        }
        Value::Duration(Duration {
            months,
            days,
            nanos,
        }) => {
            out.push(TAG_DURATION);
            out.extend_from_slice(&flip_sign_i64(*months));
            out.extend_from_slice(&flip_sign_i64(*days));
            out.extend_from_slice(&flip_sign_i64(*nanos));
        }
        Value::List(items) => {
            out.push(TAG_LIST);
            for item in items {
                write_element(out, item, depth + 1)?;
            }
            out.push(TAG_LIST_END);
        }
    }
    Ok(())
}

/// Order-preserving chunked framing for arbitrary byte strings; see the
/// module docs. Always emits at least one group, so the empty string is
/// eight zero bytes plus marker `0xf7`.
fn write_chunked(out: &mut Vec<u8>, data: &[u8]) {
    let mut i = 0;
    while data.len() - i >= GROUP {
        out.extend_from_slice(&data[i..i + GROUP]);
        out.push(MARKER_CONTINUE);
        i += GROUP;
    }
    let rem = &data[i..];
    out.extend_from_slice(rem);
    out.extend(std::iter::repeat_n(0u8, GROUP - rem.len()));
    out.push(MARKER_BASE + rem.len() as u8);
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

struct Reader<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    fn read_u8(&mut self) -> Result<u8, KeyDecodeError> {
        let b = *self
            .input
            .get(self.pos)
            .ok_or(KeyDecodeError::UnexpectedEnd)?;
        self.pos += 1;
        Ok(b)
    }

    fn peek_u8(&self) -> Result<u8, KeyDecodeError> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or(KeyDecodeError::UnexpectedEnd)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], KeyDecodeError> {
        if self.remaining() < N {
            return Err(KeyDecodeError::UnexpectedEnd);
        }
        let mut buf = [0u8; N];
        buf.copy_from_slice(&self.input[self.pos..self.pos + N]);
        self.pos += N;
        Ok(buf)
    }
}

fn unflip_sign_i64(bytes: [u8; 8]) -> i64 {
    (u64::from_be_bytes(bytes) ^ SIGN_BIT) as i64
}

fn read_element(reader: &mut Reader, depth: usize) -> Result<Value, KeyDecodeError> {
    if depth > MAX_DEPTH {
        return Err(KeyDecodeError::DepthExceeded);
    }
    let tag = reader.read_u8()?;
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_FALSE => Ok(Value::Bool(false)),
        TAG_TRUE => Ok(Value::Bool(true)),
        TAG_INT => Ok(Value::Int(unflip_sign_i64(reader.read_array()?))),
        TAG_FLOAT => {
            let enc = u64::from_be_bytes(reader.read_array()?);
            let bits = if enc & SIGN_BIT != 0 {
                enc ^ SIGN_BIT
            } else {
                !enc
            };
            let x = f64::from_bits(bits);
            if x.is_nan() {
                return Err(KeyDecodeError::NonCanonical("NaN float body"));
            }
            if bits == SIGN_BIT {
                return Err(KeyDecodeError::NonCanonical("negative-zero float body"));
            }
            Ok(Value::Float(x))
        }
        TAG_STRING => {
            let bytes = read_chunked(reader)?;
            let s = String::from_utf8(bytes).map_err(|_| KeyDecodeError::InvalidUtf8)?;
            Ok(Value::String(s))
        }
        TAG_BYTES => Ok(Value::Bytes(read_chunked(reader)?)),
        TAG_DATE => Ok(Value::Date(Date {
            days: unflip_sign_i64(reader.read_array()?),
        })),
        TAG_TIME => {
            let nanos = u64::from_be_bytes(reader.read_array()?);
            if nanos >= NANOS_PER_DAY {
                return Err(KeyDecodeError::NonCanonical("time of day out of range"));
            }
            Ok(Value::Time(Time { nanos }))
        }
        TAG_DATETIME => {
            let epoch_nanos = unflip_sign_i64(reader.read_array()?);
            let offset_minutes = (u16::from_be_bytes(reader.read_array()?) ^ 0x8000) as i16;
            if offset_minutes.abs() > MAX_OFFSET_MINUTES {
                return Err(KeyDecodeError::NonCanonical("UTC offset out of range"));
            }
            Ok(Value::DateTime(DateTime {
                epoch_nanos,
                offset_minutes,
            }))
        }
        TAG_DURATION => {
            let months = unflip_sign_i64(reader.read_array()?);
            let days = unflip_sign_i64(reader.read_array()?);
            let nanos = unflip_sign_i64(reader.read_array()?);
            Ok(Value::Duration(Duration {
                months,
                days,
                nanos,
            }))
        }
        TAG_LIST => {
            let mut items = Vec::new();
            while reader.peek_u8()? != TAG_LIST_END {
                items.push(read_element(reader, depth + 1)?);
            }
            reader.read_u8()?; // consume terminator
            Ok(Value::List(items))
        }
        other => Err(KeyDecodeError::UnknownTag(other)),
    }
}

fn read_chunked(reader: &mut Reader) -> Result<Vec<u8>, KeyDecodeError> {
    let mut out = Vec::new();
    loop {
        let group: [u8; GROUP] = reader.read_array()?;
        let marker = reader.read_u8()?;
        if marker == MARKER_CONTINUE {
            out.extend_from_slice(&group);
            continue;
        }
        if !(MARKER_BASE..MARKER_CONTINUE).contains(&marker) {
            return Err(KeyDecodeError::InvalidChunkMarker(marker));
        }
        let meaningful = (marker - MARKER_BASE) as usize; // 0..=7
        if group[meaningful..].iter().any(|&b| b != 0) {
            return Err(KeyDecodeError::NonCanonical("non-zero chunk padding"));
        }
        out.extend_from_slice(&group[..meaningful]);
        return Ok(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn enc1(v: Value) -> Vec<u8> {
        encode_key(std::slice::from_ref(&v)).expect("encode")
    }

    fn roundtrip(tuple: Vec<Value>) {
        let bytes = encode_key(&tuple).expect("encode");
        let back = decode_key(&bytes).expect("decode");
        assert_eq!(back, tuple, "round trip for {tuple:?}");
        assert_eq!(encode_key(&back).unwrap(), bytes);
    }

    /// Assert that the encodings of `values` (in the given logical order)
    /// are strictly increasing byte strings.
    fn assert_strictly_increasing(values: &[Value]) {
        for pair in values.windows(2) {
            let a = enc1(pair[0].clone());
            let b = enc1(pair[1].clone());
            assert!(
                a < b,
                "expected {:?} < {:?} but bytes {} >= {}",
                pair[0],
                pair[1],
                hex(&a),
                hex(&b)
            );
        }
    }

    #[test]
    fn fixed_width_encodings() {
        assert_eq!(hex(&enc1(Value::Null)), "01");
        assert_eq!(hex(&enc1(Value::Bool(false))), "02");
        assert_eq!(hex(&enc1(Value::Bool(true))), "03");
        assert_eq!(hex(&enc1(Value::Int(0))), "048000000000000000");
        assert_eq!(hex(&enc1(Value::Int(1))), "048000000000000001");
        assert_eq!(hex(&enc1(Value::Int(-1))), "047fffffffffffffff");
        assert_eq!(hex(&enc1(Value::Int(i64::MIN))), "040000000000000000");
        assert_eq!(hex(&enc1(Value::Int(i64::MAX))), "04ffffffffffffffff");
        assert_eq!(hex(&enc1(Value::Float(0.0))), "058000000000000000");
        assert_eq!(hex(&enc1(Value::Float(-0.0))), "058000000000000000");
        assert_eq!(hex(&enc1(Value::Float(1.0))), "05bff0000000000000");
        assert_eq!(hex(&enc1(Value::Float(-1.0))), "05400fffffffffffff");
        assert_eq!(
            hex(&enc1(Value::Date(Date { days: 0 }))),
            "088000000000000000"
        );
        assert_eq!(
            hex(&enc1(Value::Time(Time { nanos: 0 }))),
            "090000000000000000"
        );
        assert_eq!(
            hex(&enc1(Value::DateTime(DateTime {
                epoch_nanos: 0,
                offset_minutes: 0
            }))),
            "0a80000000000000008000"
        );
        assert_eq!(
            hex(&enc1(Value::Duration(Duration {
                months: 1,
                days: 2,
                nanos: 3
            }))),
            "0b800000000000000180000000000000028000000000000003"
        );
    }

    #[test]
    fn chunked_framing() {
        assert_eq!(
            hex(&enc1(Value::String(String::new()))),
            "060000000000000000f7"
        );
        assert_eq!(
            hex(&enc1(Value::String("a".into()))),
            "066100000000000000f8"
        );
        assert_eq!(
            hex(&enc1(Value::String("abcdefgh".into()))),
            "066162636465666768ff0000000000000000f7"
        );
        assert_eq!(
            hex(&enc1(Value::String("abcdefghi".into()))),
            "066162636465666768ff6900000000000000f8"
        );
        assert_eq!(hex(&enc1(Value::Bytes(vec![0, 0]))), "070000000000000000f9");
    }

    #[test]
    fn list_encoding() {
        assert_eq!(hex(&enc1(Value::List(vec![]))), "0c00");
        assert_eq!(
            hex(&enc1(Value::List(vec![
                Value::Int(1),
                Value::String("a".into())
            ]))),
            "0c04800000000000000106 6100000000000000f8 00".replace(' ', "")
        );
    }

    #[test]
    fn round_trips() {
        roundtrip(vec![]);
        roundtrip(vec![Value::Null]);
        roundtrip(vec![Value::Bool(false), Value::Bool(true)]);
        roundtrip(vec![
            Value::Int(i64::MIN),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(i64::MAX),
        ]);
        roundtrip(vec![
            Value::Float(f64::NEG_INFINITY),
            Value::Float(-1.5),
            Value::Float(0.0),
            Value::Float(f64::MIN_POSITIVE),
            Value::Float(5e-324),
            Value::Float(f64::MAX),
            Value::Float(f64::INFINITY),
        ]);
        roundtrip(vec![
            Value::String(String::new()),
            Value::String("héllo wörld".into()),
        ]);
        roundtrip(vec![
            Value::String("exactly8".into()),
            Value::String("emb\0edded".into()),
        ]);
        roundtrip(vec![
            Value::Bytes(vec![]),
            Value::Bytes((0..=255).collect()),
        ]);
        roundtrip(vec![Value::Date(Date { days: -719_468 })]);
        roundtrip(vec![Value::Time(Time {
            nanos: NANOS_PER_DAY - 1,
        })]);
        roundtrip(vec![Value::DateTime(DateTime {
            epoch_nanos: i64::MIN,
            offset_minutes: -1080,
        })]);
        roundtrip(vec![Value::Duration(Duration {
            months: -1,
            days: 2,
            nanos: -3,
        })]);
        roundtrip(vec![Value::List(vec![])]);
        roundtrip(vec![Value::List(vec![
            Value::Int(1),
            Value::List(vec![Value::String("nested".into())]),
        ])]);
        roundtrip(vec![
            Value::String("Host".into()),
            Value::Int(42),
            Value::String("eth0".into()),
        ]);
    }

    #[test]
    fn negative_zero_normalises_to_positive_zero() {
        assert_eq!(enc1(Value::Float(-0.0)), enc1(Value::Float(0.0)));
        let back = decode_key(&enc1(Value::Float(-0.0))).unwrap();
        match back.as_slice() {
            [Value::Float(x)] => assert_eq!(x.to_bits(), 0.0f64.to_bits()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn nan_is_rejected_in_keys() {
        assert_eq!(
            encode_key(&[Value::Float(f64::NAN)]),
            Err(KeyEncodeError::NanNotPermitted)
        );
        assert_eq!(
            encode_key(&[Value::List(vec![Value::Float(-f64::NAN)])]),
            Err(KeyEncodeError::NanNotPermitted)
        );
    }

    #[test]
    fn encoder_rejects_out_of_range_temporal() {
        assert_eq!(
            encode_key(&[Value::Time(Time {
                nanos: NANOS_PER_DAY
            })]),
            Err(KeyEncodeError::TimeOutOfRange(NANOS_PER_DAY))
        );
        assert_eq!(
            encode_key(&[Value::DateTime(DateTime {
                epoch_nanos: 0,
                offset_minutes: 1081
            })]),
            Err(KeyEncodeError::OffsetOutOfRange(1081))
        );
    }

    #[test]
    fn encoder_rejects_excessive_depth() {
        let mut v = Value::Int(0);
        for _ in 0..(MAX_DEPTH + 2) {
            v = Value::List(vec![v]);
        }
        assert_eq!(encode_key(&[v]), Err(KeyEncodeError::DepthExceeded));
    }

    // --- ordering ----------------------------------------------------------

    #[test]
    fn integer_order() {
        assert_strictly_increasing(&[
            Value::Int(i64::MIN),
            Value::Int(-1_000_000),
            Value::Int(-1),
            Value::Int(0),
            Value::Int(1),
            Value::Int(1_000_000),
            Value::Int(i64::MAX),
        ]);
    }

    #[test]
    fn float_order() {
        assert_strictly_increasing(&[
            Value::Float(f64::NEG_INFINITY),
            Value::Float(f64::MIN),
            Value::Float(-1.5),
            Value::Float(-f64::MIN_POSITIVE),
            Value::Float(-5e-324),
            Value::Float(0.0),
            Value::Float(5e-324),
            Value::Float(f64::MIN_POSITIVE),
            Value::Float(1.5),
            Value::Float(f64::MAX),
            Value::Float(f64::INFINITY),
        ]);
    }

    #[test]
    fn string_order_is_code_point_order() {
        assert_strictly_increasing(&[
            Value::String(String::new()),
            Value::String("a".into()),
            Value::String("a\0".into()),
            Value::String("aa".into()),
            Value::String("ab".into()),
            Value::String("abcdefg".into()),
            Value::String("abcdefgh".into()),
            Value::String("abcdefgh\0".into()),
            Value::String("abcdefghi".into()),
            Value::String("b".into()),
            Value::String("\u{e9}".into()),   // é U+00E9
            Value::String("\u{6c34}".into()), // 水
        ]);
    }

    #[test]
    fn bytes_order() {
        assert_strictly_increasing(&[
            Value::Bytes(vec![]),
            Value::Bytes(vec![0]),
            Value::Bytes(vec![0, 0]),
            Value::Bytes(vec![0, 1]),
            Value::Bytes(vec![1]),
            Value::Bytes(vec![1; 8]),
            Value::Bytes(vec![1; 9]),
            Value::Bytes(vec![0xff; 16]),
        ]);
    }

    #[test]
    fn temporal_order() {
        assert_strictly_increasing(&[
            Value::Date(Date { days: -1 }),
            Value::Date(Date { days: 0 }),
            Value::Date(Date { days: 20_000 }),
        ]);
        assert_strictly_increasing(&[
            Value::Time(Time { nanos: 0 }),
            Value::Time(Time { nanos: 1 }),
            Value::Time(Time {
                nanos: NANOS_PER_DAY - 1,
            }),
        ]);
        assert_strictly_increasing(&[
            Value::DateTime(DateTime {
                epoch_nanos: -1,
                offset_minutes: 1080,
            }),
            Value::DateTime(DateTime {
                epoch_nanos: 0,
                offset_minutes: -1080,
            }),
            Value::DateTime(DateTime {
                epoch_nanos: 0,
                offset_minutes: 0,
            }),
            Value::DateTime(DateTime {
                epoch_nanos: 0,
                offset_minutes: 60,
            }),
            Value::DateTime(DateTime {
                epoch_nanos: 1,
                offset_minutes: -1080,
            }),
        ]);
        assert_strictly_increasing(&[
            Value::Duration(Duration {
                months: 0,
                days: 0,
                nanos: -1,
            }),
            Value::Duration(Duration {
                months: 0,
                days: 0,
                nanos: 0,
            }),
            Value::Duration(Duration {
                months: 0,
                days: 1,
                nanos: -5,
            }),
            Value::Duration(Duration {
                months: 1,
                days: -9,
                nanos: 0,
            }),
        ]);
    }

    #[test]
    fn cross_type_order_follows_tags() {
        assert_strictly_increasing(&[
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Int(i64::MAX),
            Value::Float(f64::NEG_INFINITY),
            Value::String("\u{10ffff}".into()),
            Value::Bytes(vec![0xff]),
            Value::Date(Date { days: i64::MAX }),
            Value::Time(Time { nanos: 0 }),
            Value::DateTime(DateTime {
                epoch_nanos: i64::MIN,
                offset_minutes: 0,
            }),
            Value::Duration(Duration {
                months: 0,
                days: 0,
                nanos: 0,
            }),
            Value::List(vec![]),
        ]);
    }

    #[test]
    fn list_order_is_lexicographic_with_prefix_first() {
        assert_strictly_increasing(&[
            Value::List(vec![]),
            Value::List(vec![Value::Null]),
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![Value::Int(1), Value::Int(0)]),
            Value::List(vec![Value::Int(2)]),
            Value::List(vec![Value::List(vec![Value::Int(1)])]),
        ]);
    }

    #[test]
    fn tuple_order_and_prefix_scans() {
        let short = encode_key(&[Value::String("Host".into())]).unwrap();
        let long = encode_key(&[Value::String("Host".into()), Value::Int(7)]).unwrap();
        assert!(
            long.starts_with(&short),
            "tuple prefix must be a byte prefix"
        );
        assert!(short < long);
    }

    // --- strict decoding ------------------------------------------------------

    #[test]
    fn decoder_rejects_malformed_input() {
        // Unknown tag.
        assert_eq!(decode_key(&[0x00]), Err(KeyDecodeError::UnknownTag(0)));
        assert_eq!(decode_key(&[0x0d]), Err(KeyDecodeError::UnknownTag(0x0d)));
        assert_eq!(decode_key(&[0xff]), Err(KeyDecodeError::UnknownTag(0xff)));
        // Truncated bodies.
        assert_eq!(
            decode_key(&[TAG_INT, 1, 2, 3]),
            Err(KeyDecodeError::UnexpectedEnd)
        );
        assert_eq!(
            decode_key(&[TAG_STRING]),
            Err(KeyDecodeError::UnexpectedEnd)
        );
        assert_eq!(
            decode_key(&[TAG_STRING, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(KeyDecodeError::UnexpectedEnd)
        );
        // Unterminated list.
        assert_eq!(
            decode_key(&[TAG_LIST, TAG_NULL]),
            Err(KeyDecodeError::UnexpectedEnd)
        );
        // Continuation marker with nothing after it.
        let mut bytes = vec![TAG_BYTES];
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes.push(0xff);
        assert_eq!(decode_key(&bytes), Err(KeyDecodeError::UnexpectedEnd));
    }

    #[test]
    fn decoder_rejects_non_canonical_forms() {
        // Bad chunk marker.
        let mut bytes = vec![TAG_STRING];
        bytes.extend_from_slice(&[0; 8]);
        bytes.push(0x42);
        assert_eq!(
            decode_key(&bytes),
            Err(KeyDecodeError::InvalidChunkMarker(0x42))
        );
        // Non-zero padding after the meaningful bytes.
        let mut bytes = vec![TAG_STRING];
        bytes.extend_from_slice(&[b'a', 0, 0, 0, 0, 0, 0, 1]);
        bytes.push(MARKER_BASE + 1);
        assert_eq!(
            decode_key(&bytes),
            Err(KeyDecodeError::NonCanonical("non-zero chunk padding"))
        );
        // NaN float body: total-order form of +NaN (0x7ff8...) is fff8...
        let mut bytes = vec![TAG_FLOAT];
        bytes.extend_from_slice(&0xfff8_0000_0000_0000_u64.to_be_bytes());
        assert_eq!(
            decode_key(&bytes),
            Err(KeyDecodeError::NonCanonical("NaN float body"))
        );
        // Negative-zero float body: -0.0 encodes bits !0x8000... = 0x7fff...
        let mut bytes = vec![TAG_FLOAT];
        bytes.extend_from_slice(&0x7fff_ffff_ffff_ffff_u64.to_be_bytes());
        assert_eq!(
            decode_key(&bytes),
            Err(KeyDecodeError::NonCanonical("negative-zero float body"))
        );
        // Out-of-range time.
        let mut bytes = vec![TAG_TIME];
        bytes.extend_from_slice(&NANOS_PER_DAY.to_be_bytes());
        assert_eq!(
            decode_key(&bytes),
            Err(KeyDecodeError::NonCanonical("time of day out of range"))
        );
        // Out-of-range offset (+2000 minutes → 0x8000 ^ 2000).
        let mut bytes = vec![TAG_DATETIME];
        bytes.extend_from_slice(&SIGN_BIT.to_be_bytes());
        bytes.extend_from_slice(&(0x8000_u16 ^ 2000).to_be_bytes());
        assert_eq!(
            decode_key(&bytes),
            Err(KeyDecodeError::NonCanonical("UTC offset out of range"))
        );
        // Invalid UTF-8 in a string key.
        let mut bytes = vec![TAG_STRING];
        bytes.extend_from_slice(&[0xff, 0, 0, 0, 0, 0, 0, 0]);
        bytes.push(MARKER_BASE + 1);
        assert_eq!(decode_key(&bytes), Err(KeyDecodeError::InvalidUtf8));
    }

    #[test]
    fn decoder_enforces_depth_limit() {
        let bytes = vec![TAG_LIST; 200];
        assert_eq!(decode_key(&bytes), Err(KeyDecodeError::DepthExceeded));
    }
}
