//! The openCypher query layer (spec §5).
//!
//! Parser front end producing a spanned AST, binder against the schema map,
//! heuristic logical planning, and a Volcano-style iterator executor over
//! the storage traits. Conformance is measured against the openCypher TCK
//! and the pass rate is published per release.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {}
}
