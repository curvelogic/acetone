//! Content-defined chunking for prolly-tree nodes.
//!
//! A gear rolling hash runs over the serialised entry stream of each tree
//! level, and the boundary condition is evaluated **once per entry, at the
//! entry's end**, weighted by entry size: cut when
//! `gear_hash & (2^mask_bits - 1) < entry_len`. Each entry is therefore a
//! cut with probability ≈ `entry_len / 2^mask_bits`, giving a byte-based
//! expected chunk size of roughly `min_bytes + 2^mask_bits` bytes
//! independent of entry size, like per-byte CDC but with the decision
//! anchored to entry boundaries (the only places a cut may land anyway).
//!
//! Determinism / history independence (spec §3.2, normative): the gear
//! state is reset at every chunk boundary, so the chunking of a level is a
//! pure function of that level's serialised entry stream. Chunking
//! restarted at any existing boundary reproduces the remainder of the
//! chunking exactly, which is what makes incremental updates splice back
//! into unchanged chunks (see `tree.rs`).
//!
//! # Why entry-end evaluation (deviation from the Phase 0 spike; ADR-0006)
//!
//! The spike tested the boundary condition at every byte. A gear hash's
//! low `mask_bits` bits depend only on the trailing `mask_bits` bytes of
//! input, so a per-byte test inside a long *constant* region — an inner
//! entry's key bytes, which are identical at every tree level — makes the
//! same cut decision at every level. Two inner entries with long keys can
//! then split identically forever: a deterministic fixed point in which no
//! level ever has fewer chunks than the one below, and the build never
//! converges on a root. The history-independence suite's non-default-
//! parameter property found exactly this (see the committed
//! `.proptest-regressions` seed). Evaluating at entry ends removes the
//! fixed point structurally: an inner entry *ends* with the child hash, so
//! the decision window always covers bytes that change from level to
//! level. [`Chunker`] additionally never cuts a chunk with fewer than two
//! entries, so inner fan-out is at least 2 and any build of `n` entries
//! converges within `log2(n)` levels by construction.

use std::sync::LazyLock;

use crate::error::ProllyError;

/// Format ceiling on [`ChunkParams::max_bytes`]: 1 MiB. Well under any
/// store's object-size cap (the git backend defaults to 64 MiB), so a chunk
/// built with valid parameters always fits its store.
pub const MAX_CHUNK_MAX_BYTES: u32 = 1 << 20;

/// Chunking parameters. Format-defining (spec §3.2): fixed at repository
/// init, recorded in the manifest header; changing any of them changes
/// every chunk hash.
///
/// Construct via [`ChunkParams::new`], which range-checks every field; a
/// `ChunkParams` value in existence is always valid.
///
/// [`Default`] is the Phase 0 spike/test profile (min 1 KiB, mask 12 bits,
/// max 16 KiB). NOTE: this is **not** the profile shipped repositories use —
/// `acetone init` writes the repository default from
/// `acetone_graph::repo::default_chunk_params()` (max 64 KiB). Code that
/// creates or migrates a real repository must take its parameters from there
/// (or the repo's own manifest), never from this `Default`.
// `Ord` is derived so parameters can key ordered memo/dedup structures
// (e.g. fsck's cross-version canonical-tree memo, acetone-7fe); the ordering
// itself carries no meaning and is not part of any on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ChunkParams {
    /// No cut before the chunk reaches this many bytes.
    min_bytes: u32,
    /// Boundary condition (see module docs): an entry cuts when the low
    /// `mask_bits` bits of the gear hash fall below the entry's length, so
    /// eligible cut points arrive every `2^mask_bits` bytes in expectation.
    mask_bits: u32,
    /// Force a cut once the chunk reaches this many bytes (a chunk may
    /// overshoot: cuts land on entry boundaries, and never before a
    /// chunk's second entry).
    max_bytes: u32,
}

impl Default for ChunkParams {
    fn default() -> Self {
        // Mean chunk size ~ min + 2^12 ≈ 5 KiB of entry payload, i.e. the
        // ~4 KiB target of spec §3.2, order-of-magnitude. Identical to the
        // Phase 0 spike, whose behaviour these values were validated with.
        ChunkParams {
            min_bytes: 1024,
            mask_bits: 12,
            max_bytes: 16384,
        }
    }
}

impl ChunkParams {
    /// Validated constructor. Requirements (all rejected with
    /// [`ProllyError::InvalidParams`], never clamped silently):
    ///
    /// - `0 < min_bytes <= max_bytes <= MAX_CHUNK_MAX_BYTES`
    /// - `mask_bits < 64`
    pub fn new(min_bytes: u32, mask_bits: u32, max_bytes: u32) -> Result<Self, ProllyError> {
        let invalid = |reason: String| ProllyError::InvalidParams { reason };
        if min_bytes == 0 {
            return Err(invalid("min_bytes must be at least 1".into()));
        }
        if min_bytes > max_bytes {
            return Err(invalid(format!(
                "min_bytes {min_bytes} exceeds max_bytes {max_bytes}"
            )));
        }
        if max_bytes > MAX_CHUNK_MAX_BYTES {
            return Err(invalid(format!(
                "max_bytes {max_bytes} exceeds the format maximum {MAX_CHUNK_MAX_BYTES}"
            )));
        }
        if mask_bits >= 64 {
            return Err(invalid(format!("mask_bits {mask_bits} must be below 64")));
        }
        Ok(ChunkParams {
            min_bytes,
            mask_bits,
            max_bytes,
        })
    }

    /// No cut before the chunk reaches this many bytes.
    pub fn min_bytes(&self) -> u32 {
        self.min_bytes
    }

    /// Width of the boundary test window: eligible cut points arrive
    /// every `2^mask_bits` bytes in expectation (see module docs).
    pub fn mask_bits(&self) -> u32 {
        self.mask_bits
    }

    /// Force a cut once the chunk reaches this many bytes (never before a
    /// chunk's second entry).
    pub fn max_bytes(&self) -> u32 {
        self.max_bytes
    }

    fn mask(&self) -> u64 {
        // mask_bits < 64 is validated in `new`, so the shift is in range.
        (1u64 << self.mask_bits) - 1
    }
}

/// 256 pseudo-random u64s derived from a fixed seed via splitmix64. The
/// table is part of the format: regenerating it with a different seed
/// changes every chunk boundary. Identical to the Phase 0 spike's table.
static GEAR: LazyLock<[u64; 256]> = LazyLock::new(|| {
    let mut state: u64 = 0x9ae1_6a3b_2f90_404f;
    let mut table = [0u64; 256];
    for slot in table.iter_mut() {
        // splitmix64
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        *slot = z ^ (z >> 31);
    }
    table
});

/// Rolling-hash state for one chunk. Reset at every cut.
#[derive(Debug)]
pub(crate) struct Chunker {
    params: ChunkParams,
    hash: u64,
    len: u64,
    entries: u64,
}

impl Chunker {
    pub(crate) fn new(params: ChunkParams) -> Self {
        Chunker {
            params,
            hash: 0,
            len: 0,
            entries: 0,
        }
    }

    /// Feed one serialised entry. Returns true if the chunk should be cut
    /// after this entry. The decision is a pure function of the entries
    /// fed since the last [`reset`](Chunker::reset).
    pub(crate) fn feed_entry(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            self.hash = self.hash.wrapping_shl(1).wrapping_add(GEAR[b as usize]);
        }
        self.len += bytes.len() as u64;
        self.entries += 1;
        // Never cut a single-entry chunk: guarantees fan-out >= 2 at every
        // level, which bounds tree height by log2(entries) and makes the
        // non-convergent fixed point (module docs) structurally impossible
        // even under degenerate parameters. Affects only entries so large
        // they exceed min_bytes on their own.
        if self.entries < 2 {
            return false;
        }
        if self.len >= u64::from(self.params.max_bytes) {
            return true;
        }
        // Size-weighted boundary test: an entry is a cut with probability
        // ~ entry_len / 2^mask_bits, so eligible cut points arrive every
        // ~2^mask_bits bytes whatever the entry-size mix.
        self.len >= u64::from(self.params.min_bytes)
            && self.hash & self.params.mask() < bytes.len() as u64
    }

    /// Reset state for the next chunk (call after emitting a chunk).
    ///
    /// The caller constructs a fresh `Chunker` per level, so this reset is
    /// redundant with construction on the first chunk — kept deliberately
    /// (as in the validated spike): the reset being total is what makes
    /// each chunk's boundary a pure function of its own bytes.
    pub(crate) fn reset(&mut self) {
        self.hash = 0;
        self.len = 0;
        self.entries = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_validation_rejects_out_of_range() {
        assert!(ChunkParams::new(0, 12, 16384).is_err(), "zero min");
        assert!(ChunkParams::new(1024, 12, 1023).is_err(), "min > max");
        assert!(
            ChunkParams::new(1024, 12, MAX_CHUNK_MAX_BYTES + 1).is_err(),
            "max over format ceiling"
        );
        assert!(ChunkParams::new(1024, 64, 16384).is_err(), "mask_bits 64");
        assert!(
            ChunkParams::new(1024, u32::MAX, 16384).is_err(),
            "huge mask_bits"
        );
        assert!(ChunkParams::new(1, 0, 1).is_ok(), "degenerate but valid");
        assert!(ChunkParams::new(1024, 63, 16384).is_ok(), "mask_bits 63");
        assert!(ChunkParams::new(1024, 12, MAX_CHUNK_MAX_BYTES).is_ok());
    }

    #[test]
    fn default_params_are_valid() {
        let d = ChunkParams::default();
        let rebuilt = ChunkParams::new(d.min_bytes(), d.mask_bits(), d.max_bytes())
            .expect("default params pass validation");
        assert_eq!(d, rebuilt);
    }

    /// Deterministic pseudo-random entry stream for boundary tests.
    fn entries(n: usize) -> Vec<Vec<u8>> {
        let mut state: u64 = 42;
        (0..n)
            .map(|i| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let len = 40 + (state % 80) as usize;
                let mut e = vec![0u8; len];
                e[..8].copy_from_slice(&(i as u64).to_be_bytes());
                for (j, b) in e.iter_mut().enumerate().skip(8) {
                    *b = ((state >> (j % 56)) & 0xff) as u8;
                }
                e
            })
            .collect()
    }

    fn chunk_sizes(entries: &[Vec<u8>], params: ChunkParams) -> Vec<usize> {
        let mut chunker = Chunker::new(params);
        let mut sizes = Vec::new();
        let mut current = 0usize;
        for e in entries {
            current += e.len();
            if chunker.feed_entry(e) {
                sizes.push(current);
                current = 0;
                chunker.reset();
            }
        }
        if current > 0 {
            sizes.push(current);
        }
        sizes
    }

    #[test]
    fn chunk_sizes_respect_bounds_and_target() {
        let params = ChunkParams::default();
        let es = entries(50_000);
        let sizes = chunk_sizes(&es, params);
        let total: usize = sizes.iter().sum();
        assert!(
            sizes.len() > 100,
            "expected many chunks, got {}",
            sizes.len()
        );
        // Every chunk except the last respects min; every chunk respects
        // max + one max-size entry of slack.
        for (i, &s) in sizes.iter().enumerate() {
            if i + 1 < sizes.len() {
                assert!(
                    s >= params.min_bytes() as usize,
                    "chunk {i} of {s} bytes under min"
                );
            }
            assert!(
                s < params.max_bytes() as usize + 120,
                "chunk {i} of {s} bytes over max"
            );
        }
        let mean = total / sizes.len();
        assert!(
            (2048..=10240).contains(&mean),
            "mean chunk size {mean} far from ~4-5 KiB target"
        );
    }

    #[test]
    fn chunking_is_deterministic() {
        let es = entries(10_000);
        let a = chunk_sizes(&es, ChunkParams::default());
        let b = chunk_sizes(&es, ChunkParams::default());
        assert_eq!(a, b);
    }

    #[test]
    fn chunking_resynchronises_after_boundary() {
        // Chunking restarted at any chunk boundary must reproduce the
        // remaining boundaries exactly (the splice property).
        let es = entries(20_000);
        let params = ChunkParams::default();
        let sizes = chunk_sizes(&es, params);
        assert!(sizes.len() > 4);
        // Find the entry index at which the second chunk starts.
        let mut acc = 0usize;
        let first = sizes[0];
        let mut split_at = 0;
        for (i, e) in es.iter().enumerate() {
            acc += e.len();
            if acc == first {
                split_at = i + 1;
                break;
            }
        }
        assert!(split_at > 0, "no entry-aligned boundary found");
        let tail_sizes = chunk_sizes(&es[split_at..], params);
        assert_eq!(&sizes[1..], &tail_sizes[..]);
    }

    #[test]
    fn gear_table_is_pinned() {
        // The gear table is part of the format: pin its first and last
        // entries (and a middle one) so an accidental seed or algorithm
        // change fails loudly rather than silently re-chunking every map.
        assert_eq!(GEAR[0], 0xf725_441d_87a5_fe5e);
        assert_eq!(GEAR[128], 0xbd5d_871e_931b_e0e7);
        assert_eq!(GEAR[255], 0xd38a_2cdc_0c4b_ab56);
    }
}
