//! Content-addressed chunk storage for acetone (spec §3.1).
//!
//! Defines the `ChunkStore` trait — `put(&[u8]) -> Hash`, `get(&Hash) ->
//! Option<Bytes>`, plus ref and commit operations — and its reference
//! implementation over the git object database of the enclosing repository.
//! The bottom of the crate stack: nothing in acetone lives below this.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {}
}
