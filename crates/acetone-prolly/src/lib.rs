//! Prolly trees for acetone (spec §3.2).
//!
//! Ordered maps as probabilistic B-trees over the chunk store, with
//! content-defined chunk boundaries (~4 KiB mean). Provides point get, range
//! scans in both directions, batched mutation producing a new root,
//! structural diff, and three-way merge. History independence — identical
//! contents yield identical roots regardless of operation order — is
//! normative and enforced by property tests.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {}
}
