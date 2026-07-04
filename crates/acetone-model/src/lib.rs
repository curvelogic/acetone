//! The acetone data model (spec §2–§3.4).
//!
//! Node keys `(primary label, key tuple)`, edge keys, property values and
//! their encodings: memcomparable key encoding (byte order equals logical
//! order) and canonical deterministic CBOR for values. Also the schema map
//! layout and the manifest — the record of map roots that constitutes a
//! graph version. Encoding changes bump `format_version`.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {}
}
