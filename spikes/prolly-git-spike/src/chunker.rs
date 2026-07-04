//! Content-defined chunking for prolly-tree nodes.
//!
//! A gear rolling hash runs over the serialised entry stream of each tree
//! level. The hash is tested at every byte, but cuts are only taken at entry
//! boundaries: if any byte within an entry satisfies the boundary condition
//! (and the chunk has reached `min_bytes`), the chunk is cut after that
//! entry. This gives a byte-based target chunk size (mean roughly
//! `min_bytes + 2^mask_bits` bytes) independent of entry size, unlike
//! entry-count-based splitting.
//!
//! Determinism / history independence: the gear state is reset at every
//! chunk boundary, so the chunking of a level is a pure function of that
//! level's serialised entry stream. Chunking restarted at any existing
//! boundary reproduces the remainder of the chunking exactly, which is what
//! makes incremental updates splice back into unchanged chunks (see
//! `tree.rs`).

use std::sync::LazyLock;

/// Chunking parameters. Recorded in the root manifest; changing any of them
/// changes every chunk hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkParams {
    /// No cut before the chunk reaches this many bytes.
    pub min_bytes: usize,
    /// Boundary condition: low `mask_bits` bits of the gear hash all zero.
    /// Expected gap between eligible cut points is `2^mask_bits` bytes.
    pub mask_bits: u32,
    /// Force a cut once the chunk reaches this many bytes (the chunk may
    /// overshoot by at most one entry, since cuts land on entry boundaries).
    pub max_bytes: usize,
}

impl Default for ChunkParams {
    fn default() -> Self {
        // Mean chunk size ~ min + 2^12 = ~5 KiB of entry payload, i.e. the
        // ~4 KiB target of spec §3.2, order-of-magnitude.
        ChunkParams {
            min_bytes: 1024,
            mask_bits: 12,
            max_bytes: 16384,
        }
    }
}

impl ChunkParams {
    fn mask(&self) -> u64 {
        (1u64 << self.mask_bits) - 1
    }
}

/// 256 pseudo-random u64s derived from a fixed seed via splitmix64. The
/// table is part of the format: regenerating it with a different seed
/// changes every chunk boundary.
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
pub struct Chunker {
    params: ChunkParams,
    hash: u64,
    len: usize,
    cut_pending: bool,
}

impl Chunker {
    pub fn new(params: ChunkParams) -> Self {
        Chunker {
            params,
            hash: 0,
            len: 0,
            cut_pending: false,
        }
    }

    /// Feed one serialised entry. Returns true if the chunk should be cut
    /// after this entry.
    pub fn feed_entry(&mut self, bytes: &[u8]) -> bool {
        let mask = self.params.mask();
        for &b in bytes {
            self.len += 1;
            self.hash = self.hash.wrapping_shl(1).wrapping_add(GEAR[b as usize]);
            if self.len >= self.params.min_bytes && self.hash & mask == 0 {
                self.cut_pending = true;
            }
        }
        if self.len >= self.params.max_bytes {
            self.cut_pending = true;
        }
        self.cut_pending
    }

    /// Reset state for the next chunk (call after emitting a chunk).
    pub fn reset(&mut self) {
        self.hash = 0;
        self.len = 0;
        self.cut_pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                assert!(s >= params.min_bytes, "chunk {i} of {s} bytes under min");
            }
            assert!(
                s < params.max_bytes + 120,
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
}
