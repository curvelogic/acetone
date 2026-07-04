//! Hand-rolled git pack writing with explicitly chosen REF_DELTA bases
//! (ADR-0011, bead acetone-63m.13).
//!
//! git's own pack heuristics pair delta candidates by path and size, so
//! content-addressed chunk blobs — which have neither a stable path nor a
//! size correlation with their predecessor — never find the near-identical
//! prior version to delta against, and history barely compresses (Phase 0,
//! scenario 6). Acetone *knows* each rewritten chunk's predecessor at write
//! time; [`crate::GitStore::consolidate`] feeds those pairings here, and this
//! module writes a pack in which each object is stored as a REF_DELTA against
//! its chosen base whenever the delta pays for itself.
//!
//! gix cannot do this: `gix-pack`'s output path only produces whole
//! (`Kind::Base`) entries or reuses existing on-disk deltas — it creates no
//! new deltas (validation note, finding 1). So the delta encoder, pack writer
//! and index writer are hand-rolled here.
//!
//! # Production guards (ADR-0011)
//!
//! - The delta encoder indexes base positions as `u32`; a base longer than
//!   `u32::MAX` is never deltified (whole fallback), closing the silent
//!   copy-op truncation a naive port would hit on a ≥4 GiB base.
//! - Every REF_DELTA this module emits is **validated** — [`apply_delta`]
//!   must reproduce the object's exact bytes — before it is written; any
//!   mismatch falls back to a whole entry. Consolidation is therefore
//!   representation-only regardless of the quality of the base hints.
//! - Object IDs, base references, the pack trailer and the index are all
//!   sized by the repository's [`gix::hash::Kind`] (SHA-1 or SHA-256).

use std::collections::HashMap;
use std::io::Write as _;

use gix::ObjectId;
use gix::hash::Kind;

/// Longest copy op a single delta instruction can express.
const MAX_COPY: usize = 0x10000;
/// Block size for the base index (mirrors git's 16-byte rabin window).
const BLOCK: usize = 16;
/// At most this many candidate base positions are tried per block hash.
const MAX_CANDIDATES: usize = 64;

// ---------------------------------------------------------------------------
// Delta encoding (git's copy/insert instruction stream)
// ---------------------------------------------------------------------------

/// Append the little-endian 7-bit varint used for the delta size header.
fn push_size(mut n: usize, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Append one copy instruction (`offset`/`len` into the base).
fn push_copy(offset: usize, len: usize, out: &mut Vec<u8>) {
    debug_assert!((1..=MAX_COPY).contains(&len));
    let mut cmd = 0x80u8;
    let mut tail = [0u8; 7];
    let mut ntail = 0usize;
    for k in 0..4 {
        let b = ((offset >> (8 * k)) & 0xff) as u8;
        if b != 0 {
            cmd |= 1 << k;
            tail[ntail] = b;
            ntail += 1;
        }
    }
    // A size of 0x10000 is encoded as zero size bytes.
    let size = if len == MAX_COPY { 0 } else { len };
    for k in 0..3 {
        let b = ((size >> (8 * k)) & 0xff) as u8;
        if b != 0 {
            cmd |= 0x10 << k;
            tail[ntail] = b;
            ntail += 1;
        }
    }
    out.push(cmd);
    out.extend_from_slice(&tail[..ntail]);
}

/// Append pending literal bytes as insert instructions (max 127 each).
fn push_inserts(lits: &mut Vec<u8>, out: &mut Vec<u8>) {
    for chunk in lits.chunks(127) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    lits.clear();
}

/// Whether a base of `len` bytes must be stored whole rather than deltified:
/// the delta encoder indexes base positions as `u32`, so a base beyond
/// `u32::MAX` could truncate a copy offset (ADR-0011). Chunk `max_bytes` puts
/// this out of reach in practice, but a port would silently corrupt without
/// the guard.
fn base_too_large(len: usize) -> bool {
    len > u32::MAX as usize
}

/// FNV-1a over one block; the block index hash.
fn block_hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Encode `target` as a git delta against `base`: greedy block-hash matching
/// (16-byte blocks over the base, forward extension, backward extension into
/// pending literals). Always produces a valid delta; the caller decides
/// whether it is small enough to be worth storing.
///
/// Returns `None` when `base.len() > u32::MAX`, the one case where the `u32`
/// position index could truncate a copy offset (ADR-0011); the caller then
/// stores the object whole.
pub(crate) fn encode_delta(base: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    if base_too_large(base.len()) {
        return None;
    }
    let mut out = Vec::with_capacity(64);
    push_size(base.len(), &mut out);
    push_size(target.len(), &mut out);

    // Index the base at block-aligned positions.
    let mut index: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut p = 0usize;
    while p + BLOCK <= base.len() {
        index
            .entry(block_hash(&base[p..p + BLOCK]))
            .or_default()
            .push(p as u32);
        p += BLOCK;
    }

    let mut lits: Vec<u8> = Vec::new();
    let mut i = 0usize;
    while i < target.len() {
        let mut best: Option<(usize, usize)> = None; // (base offset, len)
        if i + BLOCK <= target.len()
            && let Some(cands) = index.get(&block_hash(&target[i..i + BLOCK]))
        {
            for &c in cands.iter().take(MAX_CANDIDATES) {
                let c = c as usize;
                if base[c..c + BLOCK] != target[i..i + BLOCK] {
                    continue;
                }
                let mut l = BLOCK;
                while c + l < base.len() && i + l < target.len() && base[c + l] == target[i + l] {
                    l += 1;
                }
                if best.is_none_or(|(_, bl)| l > bl) {
                    best = Some((c, l));
                }
            }
        }
        match best {
            Some((c, l)) => {
                // Extend the match backwards into pending literals.
                let mut back = 0usize;
                while back < lits.len()
                    && c > back
                    && base[c - 1 - back] == lits[lits.len() - 1 - back]
                {
                    back += 1;
                }
                lits.truncate(lits.len() - back);
                push_inserts(&mut lits, &mut out);
                let mut off = c - back;
                let mut len = l + back;
                while len > 0 {
                    let take = len.min(MAX_COPY);
                    push_copy(off, take, &mut out);
                    off += take;
                    len -= take;
                }
                i += l;
            }
            None => {
                lits.push(target[i]);
                i += 1;
            }
        }
    }
    push_inserts(&mut lits, &mut out);
    Some(out)
}

fn take1(buf: &mut &[u8]) -> Result<u8, String> {
    let (&b, rest) = buf.split_first().ok_or("truncated delta")?;
    *buf = rest;
    Ok(b)
}

fn read_size(buf: &mut &[u8]) -> Result<usize, String> {
    let mut n = 0usize;
    let mut shift = 0u32;
    loop {
        let b = take1(buf)?;
        n |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
        if shift > 35 {
            return Err("delta size varint too long".into());
        }
    }
}

/// Apply a git delta to `base` (the inverse of [`encode_delta`]; also how the
/// consolidator validates each delta against the true object bytes before
/// emitting it).
pub(crate) fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let mut p = delta;
    let bsize = read_size(&mut p)?;
    if bsize != base.len() {
        return Err(format!("delta base size {bsize} != {}", base.len()));
    }
    let tsize = read_size(&mut p)?;
    let mut out = Vec::with_capacity(tsize);
    while !p.is_empty() {
        let cmd = take1(&mut p)?;
        if cmd & 0x80 != 0 {
            let mut off = 0usize;
            let mut size = 0usize;
            for k in 0..4 {
                if cmd & (1 << k) != 0 {
                    off |= (take1(&mut p)? as usize) << (8 * k);
                }
            }
            for k in 0..3 {
                if cmd & (0x10 << k) != 0 {
                    size |= (take1(&mut p)? as usize) << (8 * k);
                }
            }
            if size == 0 {
                size = MAX_COPY;
            }
            if off + size > base.len() {
                return Err("delta copy out of bounds".into());
            }
            out.extend_from_slice(&base[off..off + size]);
        } else if cmd != 0 {
            let n = cmd as usize;
            if p.len() < n {
                return Err("truncated delta insert".into());
            }
            out.extend_from_slice(&p[..n]);
            p = &p[n..];
        } else {
            return Err("delta opcode 0 is reserved".into());
        }
    }
    if out.len() != tsize {
        return Err(format!(
            "delta produced {} bytes, expected {tsize}",
            out.len()
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Pack writing
// ---------------------------------------------------------------------------

/// One object to be written into a pack.
pub(crate) struct PackEntry {
    pub oid: ObjectId,
    pub kind: gix::object::Kind,
    /// Raw (uncompressed, headerless) object data.
    pub data: Vec<u8>,
    /// Chosen delta base (OID and its raw data). The entry is stored as a
    /// REF_DELTA only if the computed delta both beats the whole object and
    /// validates (round-trips) against the object bytes; otherwise it is
    /// stored whole.
    pub base: Option<(ObjectId, Vec<u8>)>,
}

/// Index information for one written entry.
struct IdxEntry {
    oid: ObjectId,
    offset: u64,
    crc32: u32,
}

/// A serialised pack plus its statistics; the caller pairs it with
/// [`write_idx`] to install it.
pub(crate) struct PackFile {
    pub bytes: Vec<u8>,
    /// Trailing pack checksum, also the conventional `pack-<hex>` file-name
    /// stem. Width follows the repository hash kind.
    pub trailer: ObjectId,
    index_entries: Vec<IdxEntry>,
    pub deltas: usize,
    pub whole: usize,
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(data.len() / 2 + 16),
        flate2::Compression::new(6), // git's default zlib level
    );
    enc.write_all(data).expect("write to Vec cannot fail");
    enc.finish().expect("write to Vec cannot fail")
}

fn hash_bytes(kind: Kind, data: &[u8]) -> ObjectId {
    let mut h = gix::hash::hasher(kind);
    h.update(data);
    h.try_finalize().expect("hashing bytes cannot fail")
}

fn kind_code(kind: gix::object::Kind) -> u8 {
    match kind {
        gix::object::Kind::Commit => 1,
        gix::object::Kind::Tree => 2,
        gix::object::Kind::Blob => 3,
        gix::object::Kind::Tag => 4,
    }
}

/// Append a pack entry header: 3-bit type, then the object size as a varint
/// (4 bits in the first byte, 7 per continuation byte).
fn push_obj_header(type_code: u8, size: usize, out: &mut Vec<u8>) {
    let mut byte = (type_code << 4) | (size & 0x0f) as u8;
    let mut size = size >> 4;
    while size != 0 {
        out.push(byte | 0x80);
        byte = (size & 0x7f) as u8;
        size >>= 7;
    }
    out.push(byte);
}

const OBJ_REF_DELTA: u8 = 7;

/// Decide whether `entry` should be stored as a REF_DELTA, returning the
/// validated delta bytes and base OID, or `None` to store it whole.
///
/// A delta is used only when it (a) is against a different object, (b) beats
/// the whole object by more than the base reference it costs, and (c)
/// **round-trips** back to the exact object bytes — the guard that makes
/// consolidation representation-only whatever the base hint (ADR-0011).
fn choose_delta(entry: &PackEntry, hash_len: usize) -> Option<(ObjectId, Vec<u8>)> {
    let (boid, bdata) = entry.base.as_ref()?;
    if *boid == entry.oid {
        return None; // delta against itself would be a cycle
    }
    let delta = encode_delta(bdata, &entry.data)?;
    if delta.len() + hash_len >= entry.data.len() {
        return None; // whole object is at least as small
    }
    match apply_delta(bdata, &delta) {
        Ok(rebuilt) if rebuilt == entry.data => Some((*boid, delta)),
        _ => None, // bad base hint: never emit a delta that does not verify
    }
}

/// Serialise a version-2 pack from exactly `count` entries, streamed (so a
/// whole history can be consolidated without materialising every object at
/// once). `hash_kind` is the repository object format, sizing OIDs, base
/// references and the trailer. Entries whose `base` is set are written as
/// REF_DELTA when [`choose_delta`] approves; the base must appear elsewhere in
/// the pack (the consolidator only sets bases that are in the packed set), so
/// the pack is self-contained and needs no thin completion.
pub(crate) fn write_pack(
    hash_kind: Kind,
    count: usize,
    entries: impl IntoIterator<Item = PackEntry>,
) -> Result<PackFile, String> {
    let hash_len = hash_kind.len_in_bytes();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"PACK");
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&(u32::try_from(count).map_err(|_| "too many entries")?).to_be_bytes());

    let mut index_entries = Vec::with_capacity(count);
    let mut deltas = 0usize;
    let mut whole = 0usize;
    let mut written = 0usize;
    for e in entries {
        written += 1;
        let offset = bytes.len() as u64;
        match choose_delta(&e, hash_len) {
            Some((boid, d)) => {
                push_obj_header(OBJ_REF_DELTA, d.len(), &mut bytes);
                bytes.extend_from_slice(boid.as_slice());
                bytes.extend_from_slice(&zlib(&d));
                deltas += 1;
            }
            None => {
                push_obj_header(kind_code(e.kind), e.data.len(), &mut bytes);
                bytes.extend_from_slice(&zlib(&e.data));
                whole += 1;
            }
        }
        let mut crc = crc32fast::Hasher::new();
        crc.update(&bytes[offset as usize..]);
        index_entries.push(IdxEntry {
            oid: e.oid,
            offset,
            crc32: crc.finalize(),
        });
    }
    if written != count {
        return Err(format!("pack header said {count} entries, wrote {written}"));
    }
    let trailer = hash_bytes(hash_kind, &bytes);
    bytes.extend_from_slice(trailer.as_slice());
    Ok(PackFile {
        bytes,
        trailer,
        index_entries,
        deltas,
        whole,
    })
}

/// High bit of a v2 index offset slot: set means "index into the 8-byte
/// large-offset table" rather than a literal 31-bit offset.
const LARGE_OFFSET_FLAG: u32 = 0x8000_0000;

/// Serialise a version-2 pack index (`.idx`) for `pack`, using the large
/// offset table for any entry beyond 2 GiB. git accepts this native index
/// (proven end-to-end in the tests), so no `git index-pack` subprocess is
/// needed to install a pack.
pub(crate) fn write_idx(hash_kind: Kind, pack: &PackFile) -> Result<Vec<u8>, String> {
    let mut sorted: Vec<&IdxEntry> = pack.index_entries.iter().collect();
    sorted.sort_by_key(|e| e.oid);

    let mut out = vec![0xff, b't', b'O', b'c'];
    out.extend_from_slice(&2u32.to_be_bytes());
    let mut fanout = [0u32; 256];
    for e in &sorted {
        fanout[e.oid.as_slice()[0] as usize] += 1;
    }
    let mut cum = 0u32;
    for f in fanout {
        cum += f;
        out.extend_from_slice(&cum.to_be_bytes());
    }
    for e in &sorted {
        out.extend_from_slice(e.oid.as_slice());
    }
    for e in &sorted {
        out.extend_from_slice(&e.crc32.to_be_bytes());
    }
    // Small-offset table: a literal 31-bit offset, or LARGE_OFFSET_FLAG | i
    // pointing into the large-offset table appended afterwards.
    let mut large_offsets: Vec<u64> = Vec::new();
    for e in &sorted {
        if e.offset < LARGE_OFFSET_FLAG as u64 {
            out.extend_from_slice(&(e.offset as u32).to_be_bytes());
        } else {
            let idx = u32::try_from(large_offsets.len())
                .ok()
                .filter(|i| i & LARGE_OFFSET_FLAG == 0)
                .ok_or("too many large offsets for a v2 index")?;
            out.extend_from_slice(&(LARGE_OFFSET_FLAG | idx).to_be_bytes());
            large_offsets.push(e.offset);
        }
    }
    for off in large_offsets {
        out.extend_from_slice(&off.to_be_bytes());
    }
    out.extend_from_slice(pack.trailer.as_slice());
    let idx_sha = hash_bytes(hash_kind, &out);
    out.extend_from_slice(idx_sha.as_slice());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// splitmix64.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn rand_bytes(rng: &mut Rng, len: usize) -> Vec<u8> {
        (0..len).map(|_| (rng.next() & 0xff) as u8).collect()
    }

    /// A `base` and a `target` derived from it by random small edits — the
    /// chunk-rewrite shape the encoder must handle well.
    fn edited_pair(rng: &mut Rng) -> (Vec<u8>, Vec<u8>) {
        let base_len = 256 + rng.below(8192) as usize;
        let base = rand_bytes(rng, base_len);
        let mut target = base.clone();
        for _ in 0..=rng.below(5) {
            let at = rng.below(target.len() as u64 + 1) as usize;
            match rng.below(3) {
                0 => {
                    let len = (rng.below(300) as usize).min(target.len() - at);
                    let repl = rand_bytes(rng, len);
                    target[at..at + len].copy_from_slice(&repl);
                }
                1 => {
                    let ins_len = rng.below(300) as usize;
                    let ins = rand_bytes(rng, ins_len);
                    target.splice(at..at, ins);
                }
                _ => {
                    let len = (rng.below(300) as usize).min(target.len() - at);
                    target.drain(at..at + len);
                }
            }
        }
        (base, target)
    }

    fn enc(base: &[u8], target: &[u8]) -> Vec<u8> {
        encode_delta(base, target).expect("base within u32")
    }

    #[test]
    fn delta_round_trips_on_edited_chunks() {
        let mut rng = Rng(0xde17a);
        for case in 0..300 {
            let (base, target) = edited_pair(&mut rng);
            let delta = enc(&base, &target);
            let back = apply_delta(&base, &delta).unwrap_or_else(|e| panic!("case {case}: {e}"));
            assert_eq!(back, target, "case {case} round trip");
        }
    }

    #[test]
    fn delta_is_small_for_single_record_edit() {
        let mut rng = Rng(0x51e);
        let base = rand_bytes(&mut rng, 4096);
        let mut target = base.clone();
        let repl = rand_bytes(&mut rng, 230);
        target[1800..2030].copy_from_slice(&repl);
        let delta = enc(&base, &target);
        assert_eq!(apply_delta(&base, &delta).unwrap(), target);
        assert!(
            delta.len() < 320,
            "delta of a 230-byte edit is {}",
            delta.len()
        );
    }

    #[test]
    fn delta_round_trips_on_degenerate_inputs() {
        for (base, target) in [
            (vec![], vec![]),
            (vec![], b"abc".to_vec()),
            (b"abc".to_vec(), vec![]),
            (b"abc".to_vec(), b"abc".to_vec()),
            (vec![0u8; 100_000], vec![0u8; 200_000]), // copies > 0x10000
        ] {
            let delta = enc(&base, &target);
            assert_eq!(apply_delta(&base, &delta).unwrap(), target);
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]
        #[test]
        fn delta_round_trips_on_arbitrary_bytes(
            base in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4096),
            target in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4096),
        ) {
            let delta = enc(&base, &target);
            proptest::prop_assert_eq!(apply_delta(&base, &delta).unwrap(), target);
        }
    }

    // -- end-to-end against real git ---------------------------------------

    fn git(repo: &std::path::Path, args: &[&str]) -> (bool, Vec<u8>, String) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        (
            out.status.success(),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn blob_entry(data: &[u8], base: Option<(ObjectId, Vec<u8>)>) -> PackEntry {
        let oid = gix::objs::compute_hash(Kind::Sha1, gix::object::Kind::Blob, data).expect("hash");
        PackEntry {
            oid,
            kind: gix::object::Kind::Blob,
            data: data.to_vec(),
            base,
        }
    }

    /// A whole entry and an in-pack REF_DELTA entry both read back identically
    /// through real git from a self-contained native pack + native index, and
    /// the repo passes `git fsck --strict` — no `index-pack`/`fix-thin` pass.
    #[test]
    fn git_reads_back_self_contained_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().join("repo.git");
        gix::init_bare(&repo_path).expect("init");

        let mut rng = Rng(0x9ac0);
        let whole = rand_bytes(&mut rng, 2500);
        let mut delta_target = whole.clone();
        delta_target[100..130].copy_from_slice(&rand_bytes(&mut rng, 30));

        let e_whole = blob_entry(&whole, None);
        let e_delta = blob_entry(&delta_target, Some((e_whole.oid, whole.clone())));
        let expect = [(e_whole.oid, whole), (e_delta.oid, delta_target)];

        // Base must precede its dependant in a self-contained pack.
        let pack = write_pack(Kind::Sha1, 2, [e_whole, e_delta]).expect("write pack");
        assert_eq!(
            pack.deltas, 1,
            "the edit must delta against the whole entry"
        );
        let idx = write_idx(Kind::Sha1, &pack).expect("write idx");

        let pack_dir = repo_path.join("objects/pack");
        let stem = format!("pack-{}", pack.trailer);
        std::fs::write(pack_dir.join(format!("{stem}.pack")), &pack.bytes).expect("pack");
        std::fs::write(pack_dir.join(format!("{stem}.idx")), &idx).expect("idx");

        for (oid, data) in &expect {
            let (ok, stdout, stderr) = git(&repo_path, &["cat-file", "blob", &oid.to_string()]);
            assert!(ok, "cat-file {oid}: {stderr}");
            assert_eq!(stdout, *data, "content of {oid}");
        }
        let (ok, _, stderr) = git(&repo_path, &["fsck", "--strict"]);
        assert!(ok, "fsck: {stderr}");
        let (ok, _, stderr) = git(
            &repo_path,
            &[
                "verify-pack",
                "-v",
                pack_dir.join(format!("{stem}.idx")).to_str().unwrap(),
            ],
        );
        assert!(ok, "verify-pack: {stderr}");
    }

    /// The `u32` base-size guard fires exactly at the boundary, so a ≥4 GiB
    /// base is stored whole rather than risking a truncated copy offset.
    #[test]
    fn u32_base_size_guard_boundary() {
        assert!(!base_too_large(0));
        assert!(!base_too_large(u32::MAX as usize));
        assert!(base_too_large(u32::MAX as usize + 1));
        // A normal base is well within the guard and deltifies.
        let base = vec![7u8; 64];
        assert!(encode_delta(&base, &base).is_some());
    }

    /// A delta that does not round-trip is never emitted: `choose_delta`
    /// returns `None` when the base is wrong for the target, forcing a whole
    /// entry (the representation-only guard).
    #[test]
    fn choose_delta_rejects_non_verifying_base() {
        let mut rng = Rng(0x0bad_ba5e);
        // A base totally unrelated to the target: any delta would be larger
        // than the whole object, so choose_delta declines it anyway.
        let target = rand_bytes(&mut rng, 2000);
        let unrelated = rand_bytes(&mut rng, 2000);
        let entry = blob_entry(&target, Some((blob_entry(&unrelated, None).oid, unrelated)));
        assert!(choose_delta(&entry, Kind::Sha1.len_in_bytes()).is_none());
    }
}
