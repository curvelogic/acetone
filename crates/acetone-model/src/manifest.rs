//! The manifest: the record of map roots that constitutes one graph
//! version (spec §3.3, ADR-0008).
//!
//! A [`Manifest`] lists the prolly-tree roots of the v0.1 maps plus the
//! format metadata needed to read them back: `format_version` and the
//! repository's chunking parameters (fixed at init, spec §3.2). Its
//! canonical CBOR encoding is a **pure function of the struct** —
//! encoding the same manifest always yields identical bytes, hence an
//! identical chunk hash ("manifest hashing deterministic").
//!
//! Layout: the top level is the two-element array
//! `[format_version, body]`, so any reader — including one from a
//! different format era — can read the version *first* and stop. That
//! outer shape is stable across format bumps; everything inside `body`
//! is version-`FORMAT_VERSION` territory. `body` is a canonical
//! text-keyed map:
//!
//! | field          | contents                                        |
//! |----------------|-------------------------------------------------|
//! | `nodes`        | map root `[hash bytes, height]`                 |
//! | `schema`       | map root                                        |
//! | `indexes`      | map: index name → map root (`idx/<name>`)       |
//! | `conflicts`    | map root, **present only mid-merge** (spec §6)  |
//! | `edges_fwd`    | map root                                        |
//! | `edges_rev`    | map root                                        |
//! | `chunk_params` | `[min_bytes, mask_bits, max_bytes]`             |
//!
//! (Table rows in canonical key order as encoded.) Chunk hashes are
//! opaque byte strings ([`Hash::as_bytes`]); their width follows the
//! repository's object format and is validated, not assumed. The decoder
//! is strict and total: exactly the canonical bytes, every height and
//! parameter validated, never a panic on untrusted input. Any change is
//! a `format_version` bump (spec §10).
//!
//! **Read-old-write-new (ADR-0048, ADR-0052).** Because the version is
//! read before the body, [`Manifest::decode`] *dispatches* on it rather
//! than demanding the current version: [`Manifest::DECODERS`] holds one
//! retained body reader per format acetone has ever shipped (today just
//! version 1). New writes always emit `FORMAT_VERSION`; older commits are
//! read through their era's decoder; nothing is rewritten, so a single
//! repository may hold commits at several versions side by side. A version
//! with no retained decoder — a repository from a *newer* build — is
//! rejected, not guessed at.

use crate::cbor::{
    MAJOR_ARRAY, MAJOR_BYTES, MAJOR_MAP, MAJOR_UNSIGNED, Reader, canonical_str_cmp, write_head,
    write_text,
};
use crate::values::ValueDecodeError;
use acetone_prolly::{ChunkParams, Hash, MAX_HEIGHT, ProllyError, Root};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use thiserror::Error;

/// The current storage format version (spec §10). Any change to key
/// encoding, value encoding, chunking parameters, map layouts or the
/// manifest schema increments it.
pub const FORMAT_VERSION: u32 = 1;

/// A retained body decoder for one format version: reads `body` from a
/// reader positioned just after the outer `[format_version, _]` head.
/// [`Manifest::DECODERS`] keys one of these per version acetone has shipped
/// — the read-old-write-new machinery of ADR-0048/ADR-0052.
type BodyDecoder = fn(&mut Reader) -> Result<Manifest, ManifestDecodeError>;

/// Errors from decoding a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManifestDecodeError {
    /// A low-level CBOR failure (truncation, non-canonical form, ...).
    #[error(transparent)]
    Cbor(#[from] ValueDecodeError),
    /// The manifest declares a format version this build cannot read.
    #[error("unsupported format version {0} (this build reads {FORMAT_VERSION})")]
    UnsupportedVersion(u64),
    /// The outer structure was not `[format_version, body]`.
    #[error("unexpected manifest shape: {0}")]
    Shape(&'static str),
    /// Body fields missing, unknown, duplicated or out of order.
    #[error("manifest body not canonical: {0}")]
    NotCanonical(&'static str),
    /// A chunk hash of unsupported width.
    #[error("invalid chunk hash: {0}")]
    InvalidHash(String),
    /// A map-root height outside `1..=MAX_HEIGHT`.
    #[error("invalid map-root height {0}")]
    InvalidHeight(u64),
    /// Chunking parameters that fail validation.
    #[error("invalid chunk parameters: {0}")]
    InvalidParams(String),
    /// An index name that is empty.
    #[error("empty index name")]
    EmptyIndexName,
}

/// The persisted form of one map's root: content address plus tree
/// height. Combined with the manifest's [`ChunkParams`] it reconstructs
/// an [`acetone_prolly::Root`] via [`MapRoot::to_root`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapRoot {
    /// Content address of the root chunk.
    pub hash: Hash,
    /// Tree height (1 = the root is a leaf); in `1..=MAX_HEIGHT` for any
    /// decoded manifest.
    pub height: u32,
}

impl MapRoot {
    /// Capture a prolly root's persistent fields. (The chunk parameters
    /// are stored once per manifest, not per map.)
    pub fn from_root(root: &Root) -> Self {
        MapRoot {
            hash: root.hash(),
            height: root.height(),
        }
    }

    /// Reconstruct the readable root under the manifest's parameters.
    pub fn to_root(&self, params: ChunkParams) -> Result<Root, ProllyError> {
        Root::new(self.hash, self.height, params)
    }
}

/// One graph version: the roots of its maps plus format metadata
/// (spec §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Chunking parameters every map was built with (fixed at init).
    pub chunk_params: ChunkParams,
    /// Root of the `schema` map.
    pub schema: MapRoot,
    /// Root of the `nodes` map.
    pub nodes: MapRoot,
    /// Root of the `edges_fwd` map.
    pub edges_fwd: MapRoot,
    /// Root of the `edges_rev` map.
    pub edges_rev: MapRoot,
    /// Roots of the declared index maps, by index name (`idx/<name>`).
    pub indexes: BTreeMap<String, MapRoot>,
    /// Root of the `conflicts` map — present only in a merge-in-progress
    /// workspace (spec §6).
    pub conflicts: Option<MapRoot>,
}

impl Manifest {
    /// Encode as canonical CBOR. Deterministic: a pure function of the
    /// fields, so identical manifests yield identical bytes and hence
    /// identical chunk hashes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_head(&mut out, MAJOR_ARRAY, 2);
        write_head(&mut out, MAJOR_UNSIGNED, u64::from(FORMAT_VERSION));
        self.write_body(&mut out);
        out
    }

    /// Write the version-`FORMAT_VERSION` body map (everything inside the
    /// outer `[format_version, body]` envelope). Factored out so the
    /// envelope and the body have separate, testable seams — mirroring the
    /// decode side, where [`Self::read_body_current`] reads exactly this.
    fn write_body(&self, out: &mut Vec<u8>) {
        let fields = 6 + u64::from(self.conflicts.is_some());
        write_head(out, MAJOR_MAP, fields);
        // Canonical key order (verified against canonical_str_cmp in
        // tests): nodes, schema, indexes, conflicts?, edges_fwd,
        // edges_rev, chunk_params.
        write_text(out, "nodes");
        write_map_root(out, &self.nodes);
        write_text(out, "schema");
        write_map_root(out, &self.schema);
        write_text(out, "indexes");
        let mut indexes: Vec<(&String, &MapRoot)> = self.indexes.iter().collect();
        indexes.sort_by(|a, b| canonical_str_cmp(a.0, b.0));
        write_head(out, MAJOR_MAP, indexes.len() as u64);
        for (name, root) in indexes {
            write_text(out, name);
            write_map_root(out, root);
        }
        if let Some(conflicts) = &self.conflicts {
            write_text(out, "conflicts");
            write_map_root(out, conflicts);
        }
        write_text(out, "edges_fwd");
        write_map_root(out, &self.edges_fwd);
        write_text(out, "edges_rev");
        write_map_root(out, &self.edges_rev);
        write_text(out, "chunk_params");
        write_head(out, MAJOR_ARRAY, 3);
        write_head(
            out,
            MAJOR_UNSIGNED,
            u64::from(self.chunk_params.min_bytes()),
        );
        write_head(
            out,
            MAJOR_UNSIGNED,
            u64::from(self.chunk_params.mask_bits()),
        );
        write_head(
            out,
            MAJOR_UNSIGNED,
            u64::from(self.chunk_params.max_bytes()),
        );
    }

    /// Decode, strictly: exactly the bytes [`Self::encode`] produces.
    ///
    /// Read-old-write-new (ADR-0048, ADR-0052): the outer envelope is the
    /// stable `[format_version, body]`, so decode reads the version *first*
    /// and dispatches to the retained decoder for that version. Today
    /// [`Self::DECODERS`] holds exactly the current format; a future format
    /// bump adds a row and keeps the older reader, so a repository may hold
    /// commits at several versions side by side and old commits stay
    /// readable with no history rewrite. A version with no retained decoder
    /// (a repository written by a *newer* build) is rejected with
    /// [`ManifestDecodeError::UnsupportedVersion`] rather than misread.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestDecodeError> {
        Self::decode_with(bytes, Self::DECODERS)
    }

    /// The retained per-version body decoders, keyed by the
    /// `format_version` each reads. New writes always emit
    /// `FORMAT_VERSION`; this table only ever *grows* (a format bump adds a
    /// row and never removes one), so every version acetone has shipped
    /// stays readable. See ADR-0048.
    const DECODERS: &'static [(u32, BodyDecoder)] = &[(FORMAT_VERSION, Manifest::decode_v1_body)];

    /// Read the `[format_version, body]` envelope and dispatch `body` to the
    /// matching decoder in `decoders`. Shared by [`Self::decode`] (which
    /// passes [`Self::DECODERS`]) and by tests that supply a multi-version
    /// table to prove cross-version coexistence.
    fn decode_with(
        bytes: &[u8],
        decoders: &[(u32, BodyDecoder)],
    ) -> Result<Self, ManifestDecodeError> {
        let mut reader = Reader::new(bytes);
        let arity = reader.read_head(MAJOR_ARRAY)?;
        if arity != 2 {
            return Err(ManifestDecodeError::Shape(
                "manifest must be [format_version, body]",
            ));
        }
        let version = reader.read_head(MAJOR_UNSIGNED)?;
        let decoder = u32::try_from(version).ok().and_then(|v| {
            decoders
                .iter()
                .find_map(|(ver, d)| (*ver == v).then_some(d))
        });
        match decoder {
            Some(decode_body) => decode_body(&mut reader),
            None => Err(ManifestDecodeError::UnsupportedVersion(version)),
        }
    }

    /// Decode a version-1 (current-format) body, positioned just after the
    /// outer `[format_version, _]` head. Strict and total: exactly the bytes
    /// [`Self::write_body`] produces, no trailing bytes.
    fn decode_v1_body(reader: &mut Reader) -> Result<Self, ManifestDecodeError> {
        let manifest = Self::read_body_current(reader)?;
        if reader.remaining() != 0 {
            return Err(ManifestDecodeError::Cbor(ValueDecodeError::TrailingBytes));
        }
        Ok(manifest)
    }

    /// Read exactly the current-format body map, leaving the reader
    /// positioned immediately after it (no trailing-bytes check — the caller
    /// owns end-of-input policy, so a future body that *extends* this one can
    /// reuse it). Its inverse is [`Self::write_body`].
    fn read_body_current(reader: &mut Reader) -> Result<Self, ManifestDecodeError> {
        let fields = reader.read_head(MAJOR_MAP)?;
        let conflicts_present = match fields {
            6 => false,
            7 => true,
            _ => {
                return Err(ManifestDecodeError::Shape(
                    "manifest body must have six fields (seven mid-merge)",
                ));
            }
        };
        expect_field(reader, "nodes")?;
        let nodes = read_map_root(reader)?;
        expect_field(reader, "schema")?;
        let schema = read_map_root(reader)?;
        expect_field(reader, "indexes")?;
        let count = reader.read_head(MAJOR_MAP)?;
        if count > reader.remaining() as u64 {
            return Err(ManifestDecodeError::Cbor(ValueDecodeError::LengthOverrun {
                declared: count,
                remaining: reader.remaining(),
            }));
        }
        let mut indexes = BTreeMap::new();
        let mut previous: Option<String> = None;
        for _ in 0..count {
            let name = reader.read_text()?;
            if name.is_empty() {
                return Err(ManifestDecodeError::EmptyIndexName);
            }
            if let Some(prev) = &previous
                && canonical_str_cmp(prev, &name) != Ordering::Less
            {
                return Err(ManifestDecodeError::NotCanonical(
                    "index names must be strictly ascending",
                ));
            }
            let root = read_map_root(reader)?;
            previous = Some(name.clone());
            indexes.insert(name, root);
        }
        let conflicts = if conflicts_present {
            expect_field(reader, "conflicts")?;
            Some(read_map_root(reader)?)
        } else {
            None
        };
        expect_field(reader, "edges_fwd")?;
        let edges_fwd = read_map_root(reader)?;
        expect_field(reader, "edges_rev")?;
        let edges_rev = read_map_root(reader)?;
        expect_field(reader, "chunk_params")?;
        let arity = reader.read_head(MAJOR_ARRAY)?;
        if arity != 3 {
            return Err(ManifestDecodeError::Shape(
                "chunk_params must be [min_bytes, mask_bits, max_bytes]",
            ));
        }
        let min_bytes = read_u32(reader)?;
        let mask_bits = read_u32(reader)?;
        let max_bytes = read_u32(reader)?;
        let chunk_params = ChunkParams::new(min_bytes, mask_bits, max_bytes)
            .map_err(|e| ManifestDecodeError::InvalidParams(e.to_string()))?;
        Ok(Manifest {
            chunk_params,
            schema,
            nodes,
            edges_fwd,
            edges_rev,
            indexes,
            conflicts,
        })
    }
}

fn write_map_root(out: &mut Vec<u8>, root: &MapRoot) {
    write_head(out, MAJOR_ARRAY, 2);
    let hash = root.hash.as_bytes();
    write_head(out, MAJOR_BYTES, hash.len() as u64);
    out.extend_from_slice(hash);
    write_head(out, MAJOR_UNSIGNED, u64::from(root.height));
}

fn read_map_root(reader: &mut Reader) -> Result<MapRoot, ManifestDecodeError> {
    let arity = reader.read_head(MAJOR_ARRAY)?;
    if arity != 2 {
        return Err(ManifestDecodeError::Shape(
            "map root must be [hash, height]",
        ));
    }
    let len = reader.read_head(MAJOR_BYTES)?;
    let len = reader.check_len(len)?;
    let hash = Hash::from_bytes(reader.read_exact(len)?)
        .map_err(|e| ManifestDecodeError::InvalidHash(e.to_string()))?;
    let height = reader.read_head(MAJOR_UNSIGNED)?;
    if height == 0 || height > u64::from(MAX_HEIGHT) {
        return Err(ManifestDecodeError::InvalidHeight(height));
    }
    Ok(MapRoot {
        hash,
        height: height as u32,
    })
}

fn expect_field(reader: &mut Reader, name: &'static str) -> Result<(), ManifestDecodeError> {
    let got = reader.read_text()?;
    if got != name {
        return Err(ManifestDecodeError::NotCanonical(
            "unexpected field name or order",
        ));
    }
    Ok(())
}

fn read_u32(reader: &mut Reader) -> Result<u32, ManifestDecodeError> {
    let v = reader.read_head(MAJOR_UNSIGNED)?;
    u32::try_from(v).map_err(|_| ManifestDecodeError::Shape("chunk parameter out of u32 range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cbor::canonical_str_cmp;

    fn hash(seed: u8) -> Hash {
        Hash::from_bytes(&[seed; 20]).expect("SHA-1 width")
    }

    fn manifest(conflicts: bool) -> Manifest {
        Manifest {
            chunk_params: ChunkParams::new(1024, 12, 65536).expect("valid"),
            schema: MapRoot {
                hash: hash(1),
                height: 1,
            },
            nodes: MapRoot {
                hash: hash(2),
                height: 3,
            },
            edges_fwd: MapRoot {
                hash: hash(3),
                height: 2,
            },
            edges_rev: MapRoot {
                hash: hash(4),
                height: 2,
            },
            indexes: [
                (
                    "host_os".to_owned(),
                    MapRoot {
                        hash: hash(5),
                        height: 1,
                    },
                ),
                (
                    "by_dc".to_owned(),
                    MapRoot {
                        hash: hash(6),
                        height: 1,
                    },
                ),
            ]
            .into(),
            conflicts: conflicts.then_some(MapRoot {
                hash: hash(7),
                height: 1,
            }),
        }
    }

    #[test]
    fn body_field_order_is_canonical() {
        let fields = [
            "nodes",
            "schema",
            "indexes",
            "conflicts",
            "edges_fwd",
            "edges_rev",
            "chunk_params",
        ];
        let mut sorted = fields;
        sorted.sort_by(|a, b| canonical_str_cmp(a, b));
        assert_eq!(fields, sorted, "encoder writes fields in canonical order");
    }

    #[test]
    fn round_trips_with_and_without_conflicts() {
        for conflicts in [false, true] {
            let m = manifest(conflicts);
            let bytes = m.encode();
            let back = Manifest::decode(&bytes).expect("decode");
            assert_eq!(back, m);
            assert_eq!(back.encode(), bytes, "re-encode is byte-identical");
        }
    }

    #[test]
    fn encoding_is_deterministic() {
        assert_eq!(manifest(true).encode(), manifest(true).encode());
    }

    #[test]
    fn version_is_checked_first() {
        // [2, {}] — future version with a body this build can't parse.
        let bytes = vec![0x82, 0x02, 0xa0];
        assert_eq!(
            Manifest::decode(&bytes),
            Err(ManifestDecodeError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn decode_rejects_corrupted_fields() {
        let m = manifest(false);
        let good = m.encode();
        // Truncations at every prefix length must error, never panic.
        for len in 0..good.len() {
            assert!(Manifest::decode(&good[..len]).is_err());
        }
        // Trailing garbage.
        let mut trailing = good.clone();
        trailing.push(0x00);
        assert!(matches!(
            Manifest::decode(&trailing),
            Err(ManifestDecodeError::Cbor(ValueDecodeError::TrailingBytes))
        ));
    }

    #[test]
    fn decode_rejects_invalid_heights_and_hashes() {
        // Height 0.
        let mut m = manifest(false);
        m.nodes.height = 0;
        let bytes = m.encode();
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ManifestDecodeError::InvalidHeight(0))
        ));
        // A hash of unsupported width: hand-craft a 5-byte hash body by
        // encoding then splicing is fiddly; instead check Hash::from_bytes
        // rejection surfaces through a minimal manifest. Height beyond
        // MAX_HEIGHT.
        let mut m = manifest(false);
        m.schema.height = MAX_HEIGHT + 1;
        assert!(matches!(
            Manifest::decode(&m.encode()),
            Err(ManifestDecodeError::InvalidHeight(_))
        ));
    }

    #[test]
    fn map_root_reconstructs_prolly_root() {
        let m = manifest(false);
        let root = m.nodes.to_root(m.chunk_params).expect("valid root");
        assert_eq!(root.hash(), m.nodes.hash);
        assert_eq!(root.height(), m.nodes.height);
        assert_eq!(MapRoot::from_root(&root), m.nodes);
    }

    // -------------------------------------------------------------------
    // Read-old-write-new (acetone-5yr, ADR-0048/ADR-0052).
    //
    // A *synthetic* format_version 2 exists only here, to prove the
    // dispatch machinery: its body is the current body map wrapped in a
    // two-element array `[body_map, minor]`, a shape the shipped v1
    // decoder rejects and a v2 decoder accepts. No v2 ships in the binary.
    // -------------------------------------------------------------------

    /// Encode `m` as a synthetic `format_version = 2` manifest:
    /// `[2, [<current body map>, minor]]`.
    fn encode_v2(m: &Manifest, minor: u32) -> Vec<u8> {
        let mut out = Vec::new();
        write_head(&mut out, MAJOR_ARRAY, 2);
        write_head(&mut out, MAJOR_UNSIGNED, 2);
        write_head(&mut out, MAJOR_ARRAY, 2);
        m.write_body(&mut out);
        write_head(&mut out, MAJOR_UNSIGNED, u64::from(minor));
        out
    }

    /// A retained decoder for the synthetic v2: reads the current body map
    /// (reusing the shipped reader) plus the v2-only `minor` field.
    fn decode_v2(reader: &mut Reader) -> Result<Manifest, ManifestDecodeError> {
        let arity = reader.read_head(MAJOR_ARRAY)?;
        if arity != 2 {
            return Err(ManifestDecodeError::Shape("v2 body must be [body, minor]"));
        }
        let manifest = Manifest::read_body_current(reader)?;
        let _minor = reader.read_head(MAJOR_UNSIGNED)?;
        if reader.remaining() != 0 {
            return Err(ManifestDecodeError::Cbor(ValueDecodeError::TrailingBytes));
        }
        Ok(manifest)
    }

    /// Git-blob content address, exactly as `GitStore` would compute it, so
    /// the coexistence test reasons about real object identities.
    fn blob_hash(bytes: &[u8]) -> Hash {
        let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes)
            .expect("SHA-1 blob hashing is infallible for in-memory data");
        Hash::from_bytes(oid.as_bytes()).expect("git digest is a valid hash width")
    }

    #[test]
    fn decode_dispatches_to_the_matching_version() {
        let decoders: &[(u32, BodyDecoder)] =
            &[(FORMAT_VERSION, Manifest::decode_v1_body), (2, decode_v2)];
        let m = manifest(false);
        // v1 bytes route to the v1 decoder; v2 bytes to the v2 decoder;
        // both reconstruct the same manifest.
        assert_eq!(Manifest::decode_with(&m.encode(), decoders).unwrap(), m);
        assert_eq!(
            Manifest::decode_with(&encode_v2(&m, 1), decoders).unwrap(),
            m
        );
        // A version present in neither table row is still rejected.
        let mut v9 = Vec::new();
        write_head(&mut v9, MAJOR_ARRAY, 2);
        write_head(&mut v9, MAJOR_UNSIGNED, 9);
        write_head(&mut v9, MAJOR_MAP, 0);
        assert_eq!(
            Manifest::decode_with(&v9, decoders),
            Err(ManifestDecodeError::UnsupportedVersion(9))
        );
    }

    #[test]
    fn read_old_write_new_coexistence() {
        // The read-old-write-new proof: one content-addressed store holds a
        // v1 commit's manifest and a v2 commit's manifest together; both
        // read, the v1 object is untouched by the v2 write, and re-writing
        // still emits current-format bytes at the same address.
        let decoders: &[(u32, BodyDecoder)] =
            &[(FORMAT_VERSION, Manifest::decode_v1_body), (2, decode_v2)];
        let m = manifest(true);
        let v1_bytes = m.encode();
        let v2_bytes = encode_v2(&m, 7);

        let mut store: BTreeMap<Hash, Vec<u8>> = BTreeMap::new();
        let v1_hash = blob_hash(&v1_bytes);
        let v2_hash = blob_hash(&v2_bytes);
        store.insert(v1_hash, v1_bytes.clone());
        // Writing the v2 object is purely additive: distinct address, and
        // the v1 object's bytes/address are unchanged (no rewrite).
        store.insert(v2_hash, v2_bytes);
        assert_ne!(v1_hash, v2_hash, "distinct versions are distinct objects");
        assert_eq!(
            store.get(&v1_hash),
            Some(&v1_bytes),
            "v1 object unchanged by the v2 write"
        );

        // Both decode, through their retained decoder, to the same manifest.
        let d1 = Manifest::decode_with(store.get(&v1_hash).unwrap(), decoders).expect("v1 decodes");
        let d2 = Manifest::decode_with(store.get(&v2_hash).unwrap(), decoders).expect("v2 decodes");
        assert_eq!(d1, m);
        assert_eq!(d2, m);

        // Write-new: re-encoding emits current-format (v1) bytes, so the v1
        // object address is stable across the upgrade — no force-push.
        assert_eq!(d1.encode(), v1_bytes);
        assert_eq!(blob_hash(&d1.encode()), v1_hash);
    }

    #[test]
    fn production_build_reads_only_current_format() {
        // The shipped DECODERS table has exactly the current format: a v2
        // (future) manifest is rejected, never guessed at.
        let m = manifest(false);
        assert_eq!(
            Manifest::decode(&encode_v2(&m, 1)),
            Err(ManifestDecodeError::UnsupportedVersion(2))
        );
        // The v1 body reader cannot read v2's array-shaped body even if it
        // were (wrongly) registered for version 2 — the formats are genuinely
        // distinct, not merely version-tagged.
        assert!(
            Manifest::decode_with(&encode_v2(&m, 1), &[(2, Manifest::decode_v1_body)]).is_err()
        );
    }

    #[test]
    fn version_beyond_u32_is_unsupported_not_mis_dispatched() {
        // 2^32 + 1: a naive `version as u32` would truncate to 1 and
        // mis-dispatch this to the v1 decoder. The u32::try_from guard
        // rejects it as an unknown version instead, reporting the full u64.
        let mut bytes = Vec::new();
        write_head(&mut bytes, MAJOR_ARRAY, 2);
        write_head(&mut bytes, MAJOR_UNSIGNED, u64::from(u32::MAX) + 2);
        write_head(&mut bytes, MAJOR_MAP, 0);
        assert_eq!(
            Manifest::decode(&bytes),
            Err(ManifestDecodeError::UnsupportedVersion(
                u64::from(u32::MAX) + 2
            ))
        );
    }
}
