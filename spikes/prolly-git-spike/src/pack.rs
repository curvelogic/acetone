//! EXPERIMENT (bead acetone-63m.10): hand-rolled git pack writing with
//! explicitly chosen REF_DELTA bases.
//!
//! git's own pack heuristics pair delta candidates by path/size, so
//! content-addressed chunk blobs (no stable name) never find their
//! predecessors and history barely deltas (phase 0, scenario 6). Acetone
//! *knows* each rewritten chunk's predecessor at write time; this module
//! writes pack files in which each new chunk is stored as a REF_DELTA
//! against that predecessor, using git's delta encoding (copy/insert
//! opcodes). gix cannot do this: `gix-pack`'s output path only produces
//! whole (`Kind::Base`) entries via `output::Entry::from_data` and reuses
//! existing on-disk deltas via `from_pack_entry` — it creates no new deltas
//! (re-verified against gix-pack 0.72, see the validation note).
//!
//! Not production code — see `docs/notes/pack-on-write-validation.md`.

use std::collections::HashMap;
use std::io::Write as _;

use gix::ObjectId;

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

/// FNV-1a over one block; the block index hash.
fn block_hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Encode `target` as a git delta against `base`: greedy block-hash
/// matching (16-byte blocks over the base, forward extension, backward
/// extension into pending literals). Always produces a valid delta; the
/// caller decides whether it is small enough to be worth storing.
pub fn encode_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
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
    out
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

/// Apply a git delta to `base` (the inverse of [`encode_delta`]; also how
/// the round-trip tests validate the encoder against the format).
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
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
pub struct PackEntry {
    pub oid: ObjectId,
    pub kind: gix::object::Kind,
    /// Raw (uncompressed, headerless) object data.
    pub data: Vec<u8>,
    /// Chosen delta base (OID and its raw data). The entry is stored as a
    /// REF_DELTA only if the computed delta is actually smaller than the
    /// whole object; otherwise it falls back to a whole entry.
    pub base: Option<(ObjectId, Vec<u8>)>,
}

/// Index information for one written entry.
pub struct IdxEntry {
    pub oid: ObjectId,
    pub offset: u64,
    pub crc32: u32,
}

/// A serialised pack plus everything needed to build its `.idx`.
pub struct PackFile {
    pub bytes: Vec<u8>,
    /// Trailing pack checksum (SHA-1; the spike's repos use git's default
    /// hash), also the conventional `pack-<hex>` file name stem.
    pub trailer: ObjectId,
    pub index_entries: Vec<IdxEntry>,
    pub deltas: usize,
    pub whole: usize,
    /// Total bytes of encoded (pre-zlib) deltas, for reporting.
    pub delta_bytes: u64,
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(data.len() / 2 + 16),
        flate2::Compression::new(6), // git's default zlib level
    );
    enc.write_all(data).expect("write to Vec cannot fail");
    enc.finish().expect("write to Vec cannot fail")
}

fn sha1(data: &[u8]) -> Result<ObjectId, String> {
    let mut h = gix::hash::hasher(gix::hash::Kind::Sha1);
    h.update(data);
    h.try_finalize().map_err(|e| e.to_string())
}

fn kind_code(kind: gix::object::Kind) -> u8 {
    match kind {
        gix::object::Kind::Commit => 1,
        gix::object::Kind::Tree => 2,
        gix::object::Kind::Blob => 3,
        gix::object::Kind::Tag => 4,
    }
}

/// Append a pack entry header: 3-bit type, then the object size as a
/// varint (4 bits in the first byte, 7 per continuation byte).
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

/// Serialise a version-2 pack file from `entries`. Entries whose `base` is
/// set are written as REF_DELTA when the delta pays for itself. Bases need
/// not be in the pack (a thin pack); complete it with
/// `git index-pack --stdin --fix-thin`, or index it as-is with
/// [`write_idx`] to probe git's tolerance of external bases.
pub fn write_pack(entries: &[PackEntry]) -> Result<PackFile, String> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"PACK");
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(
        &(u32::try_from(entries.len()).map_err(|_| "too many entries")?).to_be_bytes(),
    );

    let mut index_entries = Vec::with_capacity(entries.len());
    let mut deltas = 0usize;
    let mut whole = 0usize;
    let mut delta_bytes = 0u64;
    for e in entries {
        let offset = bytes.len() as u64;
        let delta = e.base.as_ref().and_then(|(boid, bdata)| {
            if *boid == e.oid {
                return None; // delta against itself would be a cycle
            }
            let d = encode_delta(bdata, &e.data);
            // Worth it only if it beats a whole entry by more than the
            // 20-byte base reference.
            (d.len() + 20 < e.data.len()).then_some((*boid, d))
        });
        match delta {
            Some((boid, d)) => {
                push_obj_header(OBJ_REF_DELTA, d.len(), &mut bytes);
                bytes.extend_from_slice(boid.as_slice());
                bytes.extend_from_slice(&zlib(&d));
                deltas += 1;
                delta_bytes += d.len() as u64;
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
    let trailer = sha1(&bytes)?;
    bytes.extend_from_slice(trailer.as_slice());
    Ok(PackFile {
        bytes,
        trailer,
        index_entries,
        deltas,
        whole,
        delta_bytes,
    })
}

/// Serialise a version-2 pack index (`.idx`) for `pack`. Only needed when
/// bypassing `git index-pack` (the external-base tolerance probe); offsets
/// beyond 2 GiB are not supported.
pub fn write_idx(pack: &PackFile) -> Result<Vec<u8>, String> {
    let mut sorted: Vec<&IdxEntry> = pack.index_entries.iter().collect();
    sorted.sort_by_key(|e| e.oid);

    let mut out = vec![0xff, b't', b'O', b'c'];
    out.extend_from_slice(&2u32.to_be_bytes());
    let mut fanout = [0u32; 256];
    for e in &sorted {
        fanout[e.oid.as_slice()[0] as usize] += 1;
    }
    let mut cum = 0u32;
    for f in &mut fanout {
        cum += *f;
        out.extend_from_slice(&cum.to_be_bytes());
        *f = cum;
    }
    for e in &sorted {
        out.extend_from_slice(e.oid.as_slice());
    }
    for e in &sorted {
        out.extend_from_slice(&e.crc32.to_be_bytes());
    }
    for e in &sorted {
        let off = u32::try_from(e.offset).map_err(|_| "pack too large for small offsets")?;
        if off & 0x8000_0000 != 0 {
            return Err("pack too large for small offsets".into());
        }
        out.extend_from_slice(&off.to_be_bytes());
    }
    out.extend_from_slice(pack.trailer.as_slice());
    let idx_sha = sha1(&out)?;
    out.extend_from_slice(idx_sha.as_slice());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    /// splitmix64, as elsewhere in the spike.
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
                    // replace a run
                    let len = (rng.below(300) as usize).min(target.len() - at);
                    let repl = rand_bytes(rng, len);
                    target[at..at + len].copy_from_slice(&repl);
                }
                1 => {
                    // insert
                    let ins_len = rng.below(300) as usize;
                    let ins = rand_bytes(rng, ins_len);
                    target.splice(at..at, ins);
                }
                _ => {
                    // delete
                    let len = (rng.below(300) as usize).min(target.len() - at);
                    target.drain(at..at + len);
                }
            }
        }
        (base, target)
    }

    #[test]
    fn delta_round_trips_on_edited_chunks() {
        let mut rng = Rng(0xde17a);
        for case in 0..300 {
            let (base, target) = edited_pair(&mut rng);
            let delta = encode_delta(&base, &target);
            let back = apply_delta(&base, &delta).unwrap_or_else(|e| panic!("case {case}: {e}"));
            assert_eq!(back, target, "case {case} round trip");
        }
    }

    #[test]
    fn delta_is_small_for_single_record_edit() {
        // The workload this experiment exists for: a ~4 KiB chunk with one
        // ~230-byte record replaced must delta down to roughly the record.
        let mut rng = Rng(0x51e);
        let base = rand_bytes(&mut rng, 4096);
        let mut target = base.clone();
        let repl = rand_bytes(&mut rng, 230);
        target[1800..2030].copy_from_slice(&repl);
        let delta = encode_delta(&base, &target);
        assert_eq!(apply_delta(&base, &delta).unwrap(), target);
        assert!(
            delta.len() < 320,
            "delta of a 230-byte edit is {} bytes",
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
            let delta = encode_delta(&base, &target);
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
            let delta = encode_delta(&base, &target);
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
        let oid = gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::object::Kind::Blob, data)
            .expect("hash");
        PackEntry {
            oid,
            kind: gix::object::Kind::Blob,
            data: data.to_vec(),
            base,
        }
    }

    fn index_pack_fix_thin(repo: &std::path::Path, pack: &[u8]) {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["index-pack", "--stdin", "--fix-thin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git index-pack");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(pack)
            .expect("pipe pack");
        let out = child.wait_with_output().expect("wait");
        assert!(
            out.status.success(),
            "index-pack failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Whole, in-pack REF_DELTA and thin (external-base) REF_DELTA entries
    /// all read back identically through real git after
    /// `index-pack --fix-thin`, and the repo passes `git fsck --strict`.
    #[test]
    fn git_reads_back_hand_written_pack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().join("repo.git");
        let repo = gix::init_bare(&repo_path).expect("init");

        let mut rng = Rng(0x9ac0);
        let external_base = rand_bytes(&mut rng, 3000);
        let base_oid = repo
            .write_blob(&external_base)
            .expect("write blob")
            .detach();

        let whole = rand_bytes(&mut rng, 2500);
        let mut inpack_target = whole.clone();
        inpack_target[100..130].copy_from_slice(&rand_bytes(&mut rng, 30));
        let mut thin_target = external_base.clone();
        thin_target.extend_from_slice(b"trailing edit");

        let e_whole = blob_entry(&whole, None);
        let e_inpack = blob_entry(&inpack_target, Some((e_whole.oid, whole.clone())));
        let e_thin = blob_entry(&thin_target, Some((base_oid, external_base.clone())));
        let expect = [
            (e_whole.oid, whole),
            (e_inpack.oid, inpack_target),
            (e_thin.oid, thin_target),
        ];

        let pack = write_pack(&[e_whole, e_inpack, e_thin]).expect("write pack");
        assert_eq!(pack.deltas, 2, "both deltas must pay for themselves");
        index_pack_fix_thin(&repo_path, &pack.bytes);

        for (oid, data) in &expect {
            let (ok, stdout, stderr) = git(&repo_path, &["cat-file", "blob", &oid.to_string()]);
            assert!(ok, "cat-file {oid}: {stderr}");
            assert_eq!(stdout, *data, "content of {oid}");
        }
        let (ok, _, stderr) = git(&repo_path, &["fsck", "--strict"]);
        assert!(ok, "fsck: {stderr}");
    }

    /// The native `.idx` writer produces an index git itself accepts
    /// (`verify-pack` + `cat-file` on a self-contained pack).
    #[test]
    fn git_accepts_hand_written_idx() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_path = dir.path().join("repo.git");
        gix::init_bare(&repo_path).expect("init");

        let mut rng = Rng(0x1dc);
        let blobs: Vec<Vec<u8>> = (0..5).map(|_| rand_bytes(&mut rng, 1000)).collect();
        let entries: Vec<PackEntry> = blobs.iter().map(|b| blob_entry(b, None)).collect();
        let oids: Vec<ObjectId> = entries.iter().map(|e| e.oid).collect();
        let pack = write_pack(&entries).expect("write pack");
        let idx = write_idx(&pack).expect("write idx");

        let pack_dir = repo_path.join("objects/pack");
        let stem = format!("pack-{}", pack.trailer);
        std::fs::write(pack_dir.join(format!("{stem}.pack")), &pack.bytes).expect("pack");
        std::fs::write(pack_dir.join(format!("{stem}.idx")), &idx).expect("idx");

        let (ok, _, stderr) = git(
            &repo_path,
            &[
                "verify-pack",
                "-v",
                pack_dir.join(format!("{stem}.idx")).to_str().expect("utf8"),
            ],
        );
        assert!(ok, "verify-pack: {stderr}");
        for (oid, data) in oids.iter().zip(&blobs) {
            let (ok, stdout, stderr) = git(&repo_path, &["cat-file", "blob", &oid.to_string()]);
            assert!(ok, "cat-file {oid}: {stderr}");
            assert_eq!(stdout, *data);
        }
    }
}
