//! Crate-internal low-level canonical CBOR machinery (RFC 8949 §4.2.1).
//!
//! Shared by [`crate::values`] (property values), [`crate::records`]
//! (node/edge records), [`crate::schema`] (schema entries) and
//! [`crate::manifest`]. The value layer deliberately rejects CBOR maps
//! ([`crate::Value`] has no map variant); the record layers need them, so
//! the primitive reader/writer lives here rather than inside `values`.
//!
//! Everything here enforces the core deterministic encoding requirements:
//! shortest-form heads, definite lengths only, and — via
//! [`canonical_str_cmp`] — the bytewise order of encoded text keys that
//! deterministic maps must be sorted in. The error vocabulary is
//! [`ValueDecodeError`], which names exactly the low-level failure modes
//! (truncation, overlong heads, indefinite lengths, ...); higher layers
//! wrap it in their own error types.

use crate::values::ValueDecodeError;
use std::cmp::Ordering;

pub(crate) const MAJOR_UNSIGNED: u8 = 0;
pub(crate) const MAJOR_NEGATIVE: u8 = 1;
pub(crate) const MAJOR_BYTES: u8 = 2;
pub(crate) const MAJOR_TEXT: u8 = 3;
pub(crate) const MAJOR_ARRAY: u8 = 4;
pub(crate) const MAJOR_MAP: u8 = 5;
pub(crate) const MAJOR_TAG: u8 = 6;
pub(crate) const MAJOR_SIMPLE: u8 = 7;

/// Cap on speculative `Vec` preallocation from attacker-controlled item
/// counts (security LOW-1, acetone-8gp). A declared count is already
/// bounded by the remaining input (every item costs at least one byte),
/// but reserving `count * size_of::<Item>()` up front amplifies hostile
/// input ~24–32×: a ~60 MiB chunk could transiently reserve gigabytes.
/// Decoders therefore reserve at most this many items and let the vector
/// grow incrementally (amortised doubling), which changes nothing about
/// what decodes — capacity is not observable in output.
pub(crate) const MAX_PREALLOC_ITEMS: usize = 1024;

pub(crate) const SIMPLE_FALSE: u8 = 0xf4;
pub(crate) const SIMPLE_TRUE: u8 = 0xf5;
pub(crate) const SIMPLE_NULL: u8 = 0xf6;
pub(crate) const HEAD_F16: u8 = 0xf9;
pub(crate) const HEAD_F32: u8 = 0xfa;
pub(crate) const HEAD_F64: u8 = 0xfb;

/// Canonical (shortest-form) CBOR head.
pub(crate) fn write_head(out: &mut Vec<u8>, major: u8, value: u64) {
    let m = major << 5;
    if value < 24 {
        out.push(m | value as u8);
    } else if value <= 0xff {
        out.push(m | 24);
        out.push(value as u8);
    } else if value <= 0xffff {
        out.push(m | 25);
        out.extend_from_slice(&(value as u16).to_be_bytes());
    } else if value <= 0xffff_ffff {
        out.push(m | 26);
        out.extend_from_slice(&(value as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&value.to_be_bytes());
    }
}

pub(crate) fn write_int(out: &mut Vec<u8>, n: i64) {
    if n >= 0 {
        write_head(out, MAJOR_UNSIGNED, n as u64);
    } else {
        // -1 - n, computed without overflow: !n reinterpreted as u64.
        write_head(out, MAJOR_NEGATIVE, !(n as u64));
    }
}

/// A definite-length text string.
pub(crate) fn write_text(out: &mut Vec<u8>, s: &str) {
    write_head(out, MAJOR_TEXT, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// The order of text strings under the bytewise comparison of their
/// canonical encodings — the order deterministic CBOR map keys must be
/// sorted in (RFC 8949 §4.2.1).
///
/// For definite-length text strings with shortest-form heads this reduces
/// to: shorter strings first, equal lengths bytewise. (The head encodes
/// the length and compares before the content in every length regime.)
pub(crate) fn canonical_str_cmp(a: &str, b: &str) -> Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

pub(crate) struct Reader<'a> {
    pub(crate) input: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(input: &'a [u8]) -> Self {
        Reader { input, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8, ValueDecodeError> {
        let b = *self
            .input
            .get(self.pos)
            .ok_or(ValueDecodeError::UnexpectedEnd)?;
        self.pos += 1;
        Ok(b)
    }

    pub(crate) fn read_exact(&mut self, n: usize) -> Result<&'a [u8], ValueDecodeError> {
        if self.remaining() < n {
            return Err(ValueDecodeError::UnexpectedEnd);
        }
        let slice = &self.input[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    pub(crate) fn read_be_u64(&mut self, width: usize) -> Result<u64, ValueDecodeError> {
        let bytes = self.read_exact(width)?;
        let mut v: u64 = 0;
        for &b in bytes {
            v = (v << 8) | u64::from(b);
        }
        Ok(v)
    }

    /// Read a CBOR head for majors 0–6, enforcing shortest form and
    /// definite length. Major 7 is handled separately (floats/simples).
    pub(crate) fn read_head(&mut self, expected_major: u8) -> Result<u64, ValueDecodeError> {
        let ib = self.read_u8()?;
        let major = ib >> 5;
        if major != expected_major {
            return Err(ValueDecodeError::Unsupported("unexpected major type"));
        }
        self.read_head_value(ib & 0x1f)
    }

    pub(crate) fn read_head_value(&mut self, ai: u8) -> Result<u64, ValueDecodeError> {
        match ai {
            0..=23 => Ok(u64::from(ai)),
            24 => {
                let v = self.read_be_u64(1)?;
                if v < 24 {
                    return Err(ValueDecodeError::NonCanonical("overlong head"));
                }
                Ok(v)
            }
            25 => {
                let v = self.read_be_u64(2)?;
                if v <= 0xff {
                    return Err(ValueDecodeError::NonCanonical("overlong head"));
                }
                Ok(v)
            }
            26 => {
                let v = self.read_be_u64(4)?;
                if v <= 0xffff {
                    return Err(ValueDecodeError::NonCanonical("overlong head"));
                }
                Ok(v)
            }
            27 => {
                let v = self.read_be_u64(8)?;
                if v <= 0xffff_ffff {
                    return Err(ValueDecodeError::NonCanonical("overlong head"));
                }
                Ok(v)
            }
            28..=30 => Err(ValueDecodeError::MalformedHead),
            31 => Err(ValueDecodeError::Unsupported("indefinite length")),
            _ => unreachable!("additional information is 5 bits"),
        }
    }

    pub(crate) fn check_len(&self, declared: u64) -> Result<usize, ValueDecodeError> {
        if declared > self.remaining() as u64 {
            return Err(ValueDecodeError::LengthOverrun {
                declared,
                remaining: self.remaining(),
            });
        }
        Ok(declared as usize)
    }

    /// Read a definite-length text string with a canonical head.
    pub(crate) fn read_text(&mut self) -> Result<String, ValueDecodeError> {
        let len = self.read_head(MAJOR_TEXT)?;
        let len = self.check_len(len)?;
        let bytes = self.read_exact(len)?;
        let s = str::from_utf8(bytes).map_err(|_| ValueDecodeError::InvalidUtf8)?;
        Ok(s.to_owned())
    }
}

pub(crate) fn unsigned_to_i64(v: u64) -> Result<i64, ValueDecodeError> {
    i64::try_from(v).map_err(|_| ValueDecodeError::IntOutOfRange)
}

pub(crate) fn negative_to_i64(v: u64) -> Result<i64, ValueDecodeError> {
    // Encoded value v represents -1 - v.
    if v > i64::MAX as u64 {
        return Err(ValueDecodeError::IntOutOfRange);
    }
    Ok(-1 - (v as i64))
}

/// Read an integer item (major 0 or 1) as i64.
pub(crate) fn read_i64(reader: &mut Reader) -> Result<i64, ValueDecodeError> {
    let ib = reader.read_u8()?;
    let major = ib >> 5;
    let v = reader.read_head_value(ib & 0x1f)?;
    match major {
        MAJOR_UNSIGNED => unsigned_to_i64(v),
        MAJOR_NEGATIVE => negative_to_i64(v),
        _ => Err(ValueDecodeError::Unsupported("expected integer")),
    }
}

// ---------------------------------------------------------------------------
// Half-precision helpers
// ---------------------------------------------------------------------------

/// Convert IEEE-754 binary16 bits to f64 exactly. NaN inputs must be
/// filtered by the caller (this returns the canonical NaN for them).
pub(crate) fn f16_bits_to_f64(bits: u16) -> f64 {
    let negative = bits & 0x8000 != 0;
    let exp = u32::from((bits >> 10) & 0x1f);
    let frac = u64::from(bits & 0x03ff);
    let magnitude = match exp {
        0 => frac as f64 * 2f64.powi(-24),
        31 => {
            if frac == 0 {
                f64::INFINITY
            } else {
                return f64::from_bits(crate::values::CANONICAL_NAN_BITS);
            }
        }
        e => (1024 + frac) as f64 * 2f64.powi(e as i32 - 25),
    };
    if negative { -magnitude } else { magnitude }
}

/// Return the binary16 bits representing `x` exactly, if any.
/// `x` must not be NaN.
pub(crate) fn f16_from_f64_exact(x: f64) -> Option<u16> {
    debug_assert!(!x.is_nan());
    let bits = x.to_bits();
    let sign = (((bits >> 63) as u16) & 1) << 15;
    if x == 0.0 {
        return Some(sign); // ±0.0, preserving the sign bit
    }
    if x.is_infinite() {
        return Some(sign | 0x7c00);
    }
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1023;
    let frac = bits & 0x000f_ffff_ffff_ffff;
    if (-14..=15).contains(&exp) {
        // Normal in f16 if the low 42 fraction bits are zero.
        if frac & ((1u64 << 42) - 1) == 0 {
            return Some(sign | (((exp + 15) as u16) << 10) | (frac >> 42) as u16);
        }
        return None;
    }
    if (-24..=-15).contains(&exp) {
        // Subnormal in f16: x = k * 2^-24 with 1 <= k < 1024.
        let significand = (1u64 << 52) | frac;
        let shift = (28 - exp) as u32; // 43..=52
        if significand & ((1u64 << shift) - 1) == 0 {
            return Some(sign | (significand >> shift) as u16);
        }
        return None;
    }
    // Out of f16 range entirely (including f64 subnormals, exp == -1023).
    None
}

/// Return `x` as f32 if the conversion is exact (bit-for-bit round trip).
/// `x` must not be NaN.
pub(crate) fn f32_from_f64_exact(x: f64) -> Option<f32> {
    debug_assert!(!x.is_nan());
    let y = x as f32;
    if (f64::from(y)).to_bits() == x.to_bits() {
        Some(y)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_str_cmp_is_length_first() {
        // "z" sorts before "aa": the one-byte head 0x61 < 0x62.
        assert_eq!(canonical_str_cmp("z", "aa"), Ordering::Less);
        assert_eq!(canonical_str_cmp("aa", "ab"), Ordering::Less);
        assert_eq!(canonical_str_cmp("aa", "aa"), Ordering::Equal);
    }

    /// Gate D freeze-audit nit (acetone-093): pin the shortest-form head
    /// width at every length-regime boundary, both sides of each edge.
    #[test]
    fn head_widths_at_every_length_regime_boundary() {
        let cases: &[(u64, usize)] = &[
            (0, 1),                       // immediate
            (23, 1),                      // last immediate
            (24, 2),                      // first 1-byte argument
            (255, 2),                     // last 1-byte argument
            (256, 3),                     // first 2-byte argument
            (65_535, 3),                  // last 2-byte argument
            (65_536, 5),                  // first 4-byte argument
            (u64::from(u32::MAX), 5),     // last 4-byte argument
            (u64::from(u32::MAX) + 1, 9), // first 8-byte argument
            (u64::MAX, 9),                // top of the range
        ];
        for &(value, width) in cases {
            let mut out = Vec::new();
            write_head(&mut out, MAJOR_UNSIGNED, value);
            assert_eq!(out.len(), width, "head width for {value}");
            let mut reader = Reader::new(&out);
            let back = reader.read_head(MAJOR_UNSIGNED).expect("canonical head");
            assert_eq!(back, value, "head round trip for {value}");
            assert_eq!(reader.remaining(), 0, "head fully consumed for {value}");
        }
    }

    /// The value at each boundary encoded one argument width up must be
    /// rejected as overlong — the boundary is exact, not fuzzy.
    #[test]
    fn boundary_values_one_width_up_are_overlong() {
        let overlong: &[&[u8]] = &[
            &[0x18, 23],                                 // 23 as 1-byte arg
            &[0x19, 0x00, 0xff],                         // 255 as 2-byte arg
            &[0x1a, 0x00, 0x00, 0xff, 0xff],             // 65535 as 4-byte arg
            &[0x1b, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff], // u32::MAX as 8-byte arg
        ];
        for bytes in overlong {
            let mut reader = Reader::new(bytes);
            assert_eq!(
                reader.read_head(MAJOR_UNSIGNED),
                Err(ValueDecodeError::NonCanonical("overlong head")),
                "must reject overlong encoding {bytes:0>2x?}"
            );
        }
    }

    #[test]
    fn canonical_str_cmp_matches_encoded_bytes() {
        // The definition: cmp(a, b) == encoded(a).cmp(encoded(b)).
        let cases = [
            "",
            "a",
            "z",
            "aa",
            "ab",
            "zz",
            "aaa",
            "\u{7f}",
            "\u{80}",
            "é",
            "日本語",
            // Around the 23/24-byte head-form boundary.
            "aaaaaaaaaaaaaaaaaaaaaaa",   // 23
            "aaaaaaaaaaaaaaaaaaaaaaaa",  // 24
            "aaaaaaaaaaaaaaaaaaaaaaaab", // 25
        ];
        for a in cases {
            for b in cases {
                let mut ea = Vec::new();
                let mut eb = Vec::new();
                write_text(&mut ea, a);
                write_text(&mut eb, b);
                assert_eq!(
                    canonical_str_cmp(a, b),
                    ea.cmp(&eb),
                    "canonical_str_cmp disagrees with encoded order for {a:?} vs {b:?}"
                );
            }
        }
    }
}
