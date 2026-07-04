//! Graph semantics for acetone (spec §2, §4, §6).
//!
//! Mutations against the node/edge maps, constraint enforcement (key
//! presence, uniqueness, existence), transactional write batching into
//! workspace roots, and merge orchestration: map-wise three-way merge,
//! post-merge validation (dangling edges, constraint re-check) and the
//! conflicts-as-data model.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {}
}
