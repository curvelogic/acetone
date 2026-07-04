//! The acetone data model (spec §2–§3.4).
//!
//! Node keys `(primary label, key tuple)`, edge keys, property values and
//! their encodings: memcomparable key encoding (byte order equals logical
//! order) and canonical deterministic CBOR for values. Also the schema map
//! layout and the manifest — the record of map roots that constitutes a
//! graph version. Encoding changes bump `format_version`.
//!
//! Two encoding modules (spec §3.4, Load-Bearing Invariant 2):
//!
//! - [`keys`] — order-preserving (memcomparable) tuple encoding, so that
//!   comparing encoded byte strings equals comparing logical key tuples and
//!   range scans over prolly trees equal label/prefix scans.
//! - [`values`] — canonical deterministic CBOR (RFC 8949 §4.2 core
//!   deterministic encoding profile) for property values.
//!
//! Both encodings are **normative**: any change to either is a
//! `format_version` bump in the manifest header (spec §10).

pub mod keys;
pub mod values;

/// Nanoseconds in a civil day; [`Time::nanos`] must be strictly below this.
pub const NANOS_PER_DAY: u64 = 86_400_000_000_000;

/// Largest permitted UTC offset magnitude in minutes (±18:00, matching
/// `java.time.ZoneOffset` and comfortably covering real-world offsets).
pub const MAX_OFFSET_MINUTES: i16 = 18 * 60;

/// A calendar date, represented as days since the epoch 1970-01-01
/// (negative for earlier dates), proleptic Gregorian.
///
/// This layer performs no calendar arithmetic and no range validation;
/// it stores and orders the day count only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    /// Days since 1970-01-01.
    pub days: i64,
}

/// A time of day, represented as nanoseconds since midnight.
///
/// Valid values are `0..NANOS_PER_DAY`; both encoders reject values outside
/// that range (leap seconds are not representable, as in most systems).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time {
    /// Nanoseconds since midnight; must be `< NANOS_PER_DAY`.
    pub nanos: u64,
}

/// An absolute instant with a recorded UTC offset.
///
/// `epoch_nanos` is nanoseconds since 1970-01-01T00:00:00Z (the instant,
/// independent of offset); `offset_minutes` records the civil offset the
/// value was written with. Two values with the same instant but different
/// offsets are distinct values; ordering (and hence key order) is by
/// `(epoch_nanos, offset_minutes)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime {
    /// Nanoseconds since the Unix epoch, UTC.
    pub epoch_nanos: i64,
    /// UTC offset in minutes; must be within `±MAX_OFFSET_MINUTES`.
    pub offset_minutes: i16,
}

/// A calendar-aware duration in the openCypher style: months, days and
/// nanoseconds are independent components (a month is not a fixed number
/// of days). No normalisation is performed between components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration {
    /// Whole months component.
    pub months: i64,
    /// Whole days component.
    pub days: i64,
    /// Sub-day component in nanoseconds.
    pub nanos: i64,
}

/// A property value (spec §2).
///
/// Values are: null, boolean, integer (i64), float (f64), string (UTF-8),
/// bytes, date, time, datetime (with offset), duration, and lists of the
/// foregoing. Nested maps are excluded from v0.1.
///
/// Spec §2 requires lists to be *homogeneous*; that is a schema/graph-layer
/// rule. This layer encodes whatever list it is given (including
/// heterogeneous ones) so that lower layers stay policy-free.
///
/// `PartialEq` is derived, so `Float(f64::NAN) != Float(f64::NAN)` follows
/// IEEE semantics. Encoded forms are compared byte-wise instead where
/// identity matters (NaN payloads are canonicalised on encode; see
/// [`values`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// The null value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit signed integer.
    Int(i64),
    /// A 64-bit IEEE-754 float.
    Float(f64),
    /// A UTF-8 string.
    String(String),
    /// An opaque byte string.
    Bytes(Vec<u8>),
    /// A calendar date.
    Date(Date),
    /// A time of day.
    Time(Time),
    /// An absolute instant with UTC offset.
    DateTime(DateTime),
    /// A months/days/nanoseconds duration.
    Duration(Duration),
    /// A list of values (homogeneity enforced at a higher layer).
    List(Vec<Value>),
}
