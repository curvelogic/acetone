//! Tree-node serialisation: deterministic, length-framed, level-tagged.
//!
//! Format (all integers big-endian):
//!
//! ```text
//! node        := level:u8 count:u32 entry*
//! leaf entry  := klen:u32 key vlen:u32 value            (level 0)
//! inner entry := klen:u32 last_key hlen:u8 hash_bytes   (level > 0)
//! ```
//!
//! The encoding is part of the format: every root hash depends on it, and
//! any change bumps `format_version` (spec §3.4/§10).
//!
//! # Hostile input
//!
//! Chunks come from untrusted repositories. Decoding:
//!
//! - returns [`ProllyError::Corrupt`] on anything malformed — never panics;
//! - never allocates from declared counts or lengths: entries are parsed
//!   one at a time against the remaining input, and keys/values are
//!   zero-copy [`Bytes`] slices of the (already size-capped) chunk;
//! - validates structure, not just framing: the level tag must equal the
//!   level the tree said this node is at, keys must be strictly ascending
//!   (so sorted-order invariants cannot be forged), inner nodes must be
//!   non-empty, and no trailing bytes may follow the declared entries.
//!
//! A parent's claim about a child (`last_key`) is verified against the
//! child's actual content on every read ([`read_node`]), so a
//! wrong-but-well-formed node yields `Corrupt`, not a wrong answer.

use acetone_store::{Bytes, ChunkStore, Hash};

use crate::error::ProllyError;
use crate::{MAX_KEY_LEN, MAX_VALUE_LEN};

/// Bytes of node header: level tag plus entry count.
pub(crate) const NODE_HEADER_LEN: usize = 5;

/// Reference to a child node: its content address plus the largest key
/// reachable beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeRef {
    pub last_key: Bytes,
    pub hash: Hash,
}

/// A decoded node.
#[derive(Debug)]
pub(crate) enum Node {
    /// Sorted `(key, value)` entries.
    Leaf(Vec<(Bytes, Bytes)>),
    /// Sorted `(last_key, child hash)` references; never empty.
    Inner(Vec<NodeRef>),
}

impl Node {
    /// The largest key in (or beneath) this node; `None` only for the
    /// canonical empty leaf.
    pub(crate) fn last_key(&self) -> Option<&[u8]> {
        match self {
            Node::Leaf(entries) => entries.last().map(|(k, _)| k.as_ref()),
            Node::Inner(refs) => refs.last().map(|r| r.last_key.as_ref()),
        }
    }

    /// The smallest key in (or beneath) this node; `None` only for the
    /// canonical empty leaf.
    pub(crate) fn first_key(&self) -> Option<&[u8]> {
        match self {
            Node::Leaf(entries) => entries.first().map(|(k, _)| k.as_ref()),
            Node::Inner(refs) => refs.first().map(|r| r.last_key.as_ref()),
        }
    }
}

fn corrupt(reason: impl Into<String>) -> ProllyError {
    ProllyError::corrupt("tree node", reason)
}

/// Slice `n` bytes off the front of the chunk at `*pos`, zero-copy.
/// Allocation-free and bounded by the actual input, whatever lengths the
/// chunk declares.
fn take(data: &Bytes, pos: &mut usize, n: usize) -> Result<Bytes, ProllyError> {
    let end = pos
        .checked_add(n)
        .ok_or_else(|| corrupt("length overflow"))?;
    if end > data.len() {
        return Err(corrupt("truncated node"));
    }
    let slice = data.slice(*pos..end);
    *pos = end;
    Ok(slice)
}

fn take_u8(data: &Bytes, pos: &mut usize) -> Result<u8, ProllyError> {
    Ok(take(data, pos, 1)?[0])
}

fn take_u32(data: &Bytes, pos: &mut usize) -> Result<u32, ProllyError> {
    let b = take(data, pos, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Decode one node, validating that its level tag equals `expect_level`
/// (the level the parent — or the root descriptor — says it is at).
pub(crate) fn decode_node(data: &Bytes, expect_level: u8) -> Result<Node, ProllyError> {
    let mut pos = 0usize;
    let level = take_u8(data, &mut pos)?;
    if level != expect_level {
        return Err(corrupt(format!(
            "level tag {level} where the tree requires level {expect_level}"
        )));
    }
    let count = take_u32(data, &mut pos)? as usize;
    let node = if level == 0 {
        let mut entries: Vec<(Bytes, Bytes)> = Vec::new();
        for _ in 0..count {
            let klen = take_u32(data, &mut pos)? as usize;
            let key = take(data, &mut pos, klen)?;
            let vlen = take_u32(data, &mut pos)? as usize;
            let value = take(data, &mut pos, vlen)?;
            if let Some((prev, _)) = entries.last()
                && prev.as_ref() >= key.as_ref()
            {
                return Err(corrupt("leaf keys not strictly ascending"));
            }
            entries.push((key, value));
        }
        Node::Leaf(entries)
    } else {
        if count == 0 {
            return Err(corrupt("empty inner node"));
        }
        let mut refs: Vec<NodeRef> = Vec::new();
        for _ in 0..count {
            let klen = take_u32(data, &mut pos)? as usize;
            let last_key = take(data, &mut pos, klen)?;
            let hlen = take_u8(data, &mut pos)? as usize;
            let hash_bytes = take(data, &mut pos, hlen)?;
            let hash = Hash::from_bytes(&hash_bytes)
                .map_err(|e| corrupt(format!("bad child hash: {e}")))?;
            if let Some(prev) = refs.last()
                && prev.last_key.as_ref() >= last_key.as_ref()
            {
                return Err(corrupt("inner keys not strictly ascending"));
            }
            refs.push(NodeRef { last_key, hash });
        }
        Node::Inner(refs)
    };
    if pos != data.len() {
        return Err(corrupt("trailing bytes after declared entries"));
    }
    Ok(node)
}

/// Fetch and decode the node at `hash`, validating:
///
/// - the chunk exists ([`ProllyError::MissingChunk`] otherwise — dangling
///   references are detected, never followed silently);
/// - its level tag equals `expect_level` (bounds every descent by the
///   declared height: levels strictly decrease, so no reference cycle can
///   trap a walk);
/// - when the parent declared this child's boundary, the child's actual
///   last key equals it (`expect_last_key`);
/// - when a lower bound is known from the preceding sibling, the child's
///   first key lies strictly above it (`min_key_exclusive`), so a node
///   cannot smuggle keys into a range it does not own.
pub(crate) fn read_node<S: ChunkStore>(
    store: &S,
    hash: &Hash,
    expect_level: u8,
    expect_last_key: Option<&[u8]>,
    min_key_exclusive: Option<&[u8]>,
) -> Result<Node, ProllyError> {
    let data = store
        .get(hash)?
        .ok_or(ProllyError::MissingChunk { hash: *hash })?;
    let node = decode_node(&data, expect_level)?;
    if let Some(want) = expect_last_key {
        match node.last_key() {
            Some(got) if got == want => {}
            _ => {
                return Err(corrupt(format!(
                    "chunk {hash} does not end at the key its parent declares"
                )));
            }
        }
    }
    if let Some(min) = min_key_exclusive
        && let Some(first) = node.first_key()
        && first <= min
    {
        return Err(corrupt(format!(
            "chunk {hash} contains keys below its position in the tree"
        )));
    }
    Ok(node)
}

/// Append the deterministic serialisation of one leaf entry, rejecting
/// over-length keys/values (never truncating them into the u32 frames).
pub(crate) fn encode_leaf_entry(
    key: &[u8],
    value: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), ProllyError> {
    let klen = u32::try_from(key.len()).map_err(|_| ProllyError::KeyTooLong {
        len: key.len(),
        max: MAX_KEY_LEN,
    })?;
    let vlen = u32::try_from(value.len()).map_err(|_| ProllyError::ValueTooLong {
        len: value.len(),
        max: MAX_VALUE_LEN,
    })?;
    out.extend_from_slice(&klen.to_be_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&vlen.to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}

/// Append the deterministic serialisation of one inner entry.
pub(crate) fn encode_inner_entry(
    last_key: &[u8],
    hash: &Hash,
    out: &mut Vec<u8>,
) -> Result<(), ProllyError> {
    let klen = u32::try_from(last_key.len()).map_err(|_| ProllyError::KeyTooLong {
        len: last_key.len(),
        max: MAX_KEY_LEN,
    })?;
    out.extend_from_slice(&klen.to_be_bytes());
    out.extend_from_slice(last_key);
    let digest = hash.as_bytes();
    // Digest widths are 20 (SHA-1) or 32 (SHA-256) by construction of
    // `Hash`; the guard keeps a future width honest rather than truncating.
    let hlen = u8::try_from(digest.len()).map_err(|_| {
        ProllyError::corrupt("tree node", "digest width exceeds the u8 length frame")
    })?;
    out.push(hlen);
    out.extend_from_slice(digest);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> Hash {
        Hash::from_bytes(&[byte; 20]).expect("20-byte digest is a valid SHA-1 width")
    }

    fn leaf_chunk(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut out = vec![0u8];
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (k, v) in entries {
            encode_leaf_entry(k, v, &mut out).expect("encode leaf entry");
        }
        out
    }

    fn inner_chunk(level: u8, refs: &[(&[u8], Hash)]) -> Vec<u8> {
        let mut out = vec![level];
        out.extend_from_slice(&(refs.len() as u32).to_be_bytes());
        for (k, h) in refs {
            encode_inner_entry(k, h, &mut out).expect("encode inner entry");
        }
        out
    }

    #[test]
    fn leaf_round_trips() {
        let chunk = Bytes::from(leaf_chunk(&[(b"a", b"1"), (b"b", b""), (b"cc", b"33")]));
        let node = decode_node(&chunk, 0).expect("decode");
        let Node::Leaf(entries) = node else {
            panic!("expected leaf")
        };
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0.as_ref(), b"a");
        assert_eq!(entries[1].1.as_ref(), b"");
        assert_eq!(entries[2].0.as_ref(), b"cc");
    }

    #[test]
    fn inner_round_trips() {
        let chunk = Bytes::from(inner_chunk(2, &[(b"m", hash(1)), (b"z", hash(2))]));
        let node = decode_node(&chunk, 2).expect("decode");
        let Node::Inner(refs) = node else {
            panic!("expected inner")
        };
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].last_key.as_ref(), b"m");
        assert_eq!(refs[1].hash, hash(2));
    }

    #[test]
    fn empty_leaf_is_valid_and_empty_inner_is_not() {
        let leaf = Bytes::from(leaf_chunk(&[]));
        assert!(matches!(
            decode_node(&leaf, 0),
            Ok(Node::Leaf(entries)) if entries.is_empty()
        ));

        let mut inner = vec![1u8];
        inner.extend_from_slice(&0u32.to_be_bytes());
        assert!(decode_node(&Bytes::from(inner), 1).is_err());
    }

    #[test]
    fn wrong_level_tag_is_corrupt() {
        let chunk = Bytes::from(leaf_chunk(&[(b"a", b"1")]));
        assert!(decode_node(&chunk, 1).is_err());
        let chunk = Bytes::from(inner_chunk(3, &[(b"a", hash(1))]));
        assert!(decode_node(&chunk, 2).is_err());
    }

    #[test]
    fn unsorted_or_duplicate_keys_are_corrupt() {
        let chunk = Bytes::from(leaf_chunk(&[(b"b", b"1"), (b"a", b"2")]));
        assert!(decode_node(&chunk, 0).is_err(), "descending keys");
        let chunk = Bytes::from(leaf_chunk(&[(b"a", b"1"), (b"a", b"2")]));
        assert!(decode_node(&chunk, 0).is_err(), "duplicate keys");
        let chunk = Bytes::from(inner_chunk(1, &[(b"b", hash(1)), (b"b", hash(2))]));
        assert!(decode_node(&chunk, 1).is_err(), "duplicate inner keys");
    }

    #[test]
    fn truncation_anywhere_is_corrupt_not_panic() {
        let full = leaf_chunk(&[(b"alpha", b"12345"), (b"beta", b"67890")]);
        for cut in 0..full.len() {
            let chunk = Bytes::from(full[..cut].to_vec());
            assert!(
                decode_node(&chunk, 0).is_err(),
                "truncation at {cut} must be corrupt"
            );
        }
        let full = inner_chunk(1, &[(b"alpha", hash(7))]);
        for cut in 0..full.len() {
            let chunk = Bytes::from(full[..cut].to_vec());
            assert!(
                decode_node(&chunk, 1).is_err(),
                "truncation at {cut} must be corrupt"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_corrupt() {
        let mut chunk = leaf_chunk(&[(b"a", b"1")]);
        chunk.push(0);
        assert!(decode_node(&Bytes::from(chunk), 0).is_err());
    }

    #[test]
    fn huge_declared_count_is_corrupt_without_allocation() {
        // A tiny chunk declaring u32::MAX entries must fail on the missing
        // first entry, not allocate for the declared count.
        let mut chunk = vec![0u8];
        chunk.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode_node(&Bytes::from(chunk), 0).is_err());
    }

    #[test]
    fn huge_declared_length_is_corrupt() {
        let mut chunk = vec![0u8];
        chunk.extend_from_slice(&1u32.to_be_bytes());
        chunk.extend_from_slice(&u32::MAX.to_be_bytes()); // klen
        chunk.extend_from_slice(b"tiny");
        assert!(decode_node(&Bytes::from(chunk), 0).is_err());
    }

    #[test]
    fn bad_digest_width_is_corrupt() {
        let mut chunk = vec![1u8];
        chunk.extend_from_slice(&1u32.to_be_bytes());
        chunk.extend_from_slice(&1u32.to_be_bytes());
        chunk.push(b'k');
        chunk.push(5); // hlen: not a valid digest width
        chunk.extend_from_slice(&[0xaa; 5]);
        assert!(decode_node(&Bytes::from(chunk), 1).is_err());
    }
}
