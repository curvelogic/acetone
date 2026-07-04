//! Consolidation (gc) contract tests (ADR-0011, bead acetone-63m.13).
//!
//! The load-bearing property is **representation-only**: consolidation must
//! preserve every object's bytes and address exactly while changing only how
//! they are stored. These tests build a multi-version repository, consolidate,
//! and assert the whole object set is byte-for-byte identical (via git's own
//! object walk and via the store's read path), that deltas were actually
//! chosen, that pruning shrank the store, and that git accepts the result.

mod common;

use std::collections::BTreeSet;

use acetone_store::{
    ChunkStore, CommitStore, ConsolidateOptions, GitStore, GitStoreOptions, Hash, NewCommit,
    ObjectFormat, RefStore,
};
use common::{git, new_store, repo_path};

/// splitmix64, for deterministic chunk bodies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// A deterministic ~4 KiB chunk `i` at version `v`: version 0 is a random
/// body, each later version rewrites one 32-byte window — the near-identical
/// successive-version shape consolidation exists to delta.
fn make_chunk(i: usize, v: usize) -> Vec<u8> {
    let mut rng = Rng(0x100 + i as u64);
    let mut data: Vec<u8> = (0..4096).map(|_| (rng.next() & 0xff) as u8).collect();
    for k in 0..v {
        let pos = (i * 7 + k * 131) % (4096 - 32);
        let mut r = Rng(0x9999 + (v * 1000 + i * 10 + k) as u64);
        for b in &mut data[pos..pos + 32] {
            *b = (r.next() & 0xff) as u8;
        }
    }
    data
}

/// The set of every object OID present in the repository, via git's own walk.
fn all_object_oids(repo: &std::path::Path) -> BTreeSet<String> {
    git(
        repo,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// Count loose object files (two-hex shard dirs under objects/).
fn loose_object_count(repo: &std::path::Path) -> usize {
    let objects = repo.join("objects");
    let mut count = 0;
    for shard in std::fs::read_dir(&objects).into_iter().flatten().flatten() {
        let name = shard.file_name();
        let name = name.to_string_lossy();
        if name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            count += std::fs::read_dir(shard.path())
                .into_iter()
                .flatten()
                .count();
        }
    }
    count
}

/// Build `versions` versions of `chunks` chunks each, anchoring every version
/// in a commit on a `refs/heads/main` chain, recording the (new→previous)
/// predecessor hint for each rewritten chunk. Returns every chunk's
/// (hash, bytes) across all versions.
fn build_history(store: &GitStore, chunks: usize, versions: usize) -> Vec<(Hash, Vec<u8>)> {
    let mut all: Vec<(Hash, Vec<u8>)> = Vec::new();
    let mut prev: Vec<Hash> = Vec::new();
    let mut ref_current: Option<Hash> = None;
    for v in 0..versions {
        let mut cur = Vec::with_capacity(chunks);
        let mut hints = Vec::new();
        for i in 0..chunks {
            let data = make_chunk(i, v);
            let hash = store.put(&data).expect("put chunk");
            if let Some(&base) = prev.get(i) {
                hints.push((hash, base));
            }
            all.push((hash, data));
            cur.push(hash);
        }
        store.record_base_hints(&hints).expect("record hints");
        let manifest = format!("version {v}\n");
        let message = format!("v{v}");
        let mut new = NewCommit::new(manifest.as_bytes(), "history\n", &message);
        let parents: Vec<Hash> = ref_current.into_iter().collect();
        new.parents = &parents;
        new.anchors = &cur;
        let commit = store.create_commit(&new).expect("commit");
        store
            .write_ref("refs/heads/main", ref_current.as_ref(), &commit)
            .expect("write ref");
        ref_current = Some(commit);
        prev = cur;
    }
    all
}

#[test]
fn consolidation_is_representation_only_and_deltifies() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let all = build_history(&store, 16, 6);

    let before = all_object_oids(&repo);
    let loose_before = loose_object_count(&repo);

    let stats = store
        .consolidate(ConsolidateOptions::default())
        .expect("consolidate");

    // Deltas were actually chosen (the whole point) and every reachable
    // object was packed.
    assert_eq!(
        stats.objects,
        before.len(),
        "packed the whole reachable set"
    );
    assert!(stats.deltas > 0, "predecessor hints must produce deltas");
    assert!(stats.pruned_loose > 0, "loose objects must be pruned");

    // Representation-only: git sees exactly the same object set afterwards —
    // no OID changed, nothing was lost or added.
    let store = GitStore::open(&repo).expect("reopen");
    let after = all_object_oids(&repo);
    assert_eq!(before, after, "the object set must be byte-identical");

    // Every chunk still reads back byte-for-byte through the store.
    for (hash, data) in &all {
        let got = store.get(hash).expect("get").expect("present");
        assert_eq!(
            got.as_ref(),
            data.as_slice(),
            "chunk {hash} content preserved"
        );
    }

    // The tip commit and its manifest survive unchanged.
    let tip = store
        .read_ref("refs/heads/main")
        .expect("read ref")
        .expect("ref present");
    let commit = store
        .read_commit(&tip)
        .expect("read commit")
        .expect("present");
    assert_eq!(commit.manifest.as_ref(), b"version 5\n");

    // Pruning actually shrank the loose store, and git is happy with the pack.
    assert!(
        loose_object_count(&repo) < loose_before,
        "loose objects should have dropped"
    );
    git(&repo, &["fsck", "--strict"]);
}

#[test]
fn consolidation_is_representation_only_on_a_sha256_repo() {
    // The pack/index writers are hash-kind parameterised; exercise the 32-byte
    // OID path end to end (base refs, oid table, both trailers) against a real
    // SHA-256 repository.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo.git");
    let mut options = GitStoreOptions::default();
    options.object_format = ObjectFormat::Sha256;
    let store = GitStore::create_with(&repo, options).expect("create sha256 store");
    let all = build_history(&store, 8, 4);

    let before = all_object_oids(&repo);
    let stats = store
        .consolidate(ConsolidateOptions::default())
        .expect("consolidate");
    assert_eq!(stats.objects, before.len());
    assert!(stats.deltas > 0, "sha256 deltas must be chosen too");

    let store = GitStore::open(&repo).expect("reopen");
    assert_eq!(
        before,
        all_object_oids(&repo),
        "representation-only (sha256)"
    );
    for (hash, data) in &all {
        assert_eq!(store.get(hash).unwrap().unwrap().as_ref(), data.as_slice());
    }
    git(&repo, &["fsck", "--strict"]);
}

#[test]
fn consolidation_without_hints_stores_whole_and_preserves_bytes() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);

    // A repository with no recorded hints: build history but skip the hint
    // log by using a store that never records (record nothing).
    let mut all = Vec::new();
    let mut ref_current: Option<Hash> = None;
    for v in 0..3 {
        let data = make_chunk(0, v);
        let hash = store.put(&data).expect("put");
        all.push((hash, data));
        let mut new = NewCommit::new(b"m\n", "s\n", "c");
        let parents: Vec<Hash> = ref_current.into_iter().collect();
        new.parents = &parents;
        let anchors = [hash];
        new.anchors = &anchors;
        let commit = store.create_commit(&new).expect("commit");
        store
            .write_ref("refs/heads/main", ref_current.as_ref(), &commit)
            .expect("ref");
        ref_current = Some(commit);
    }

    let before = all_object_oids(&repo);
    let stats = store
        .consolidate(ConsolidateOptions::default())
        .expect("consolidate");
    assert_eq!(stats.deltas, 0, "no hints ⇒ no deltas");
    assert_eq!(stats.whole, stats.objects);

    let store = GitStore::open(&repo).expect("reopen");
    assert_eq!(before, all_object_oids(&repo), "representation-only");
    for (hash, data) in &all {
        assert_eq!(store.get(hash).unwrap().unwrap().as_ref(), data.as_slice());
    }
    git(&repo, &["fsck", "--strict"]);
}

/// Count `*.pack` files in the repository's pack directory.
fn pack_count(repo: &std::path::Path) -> usize {
    std::fs::read_dir(repo.join("objects/pack"))
        .expect("pack dir")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
        .count()
}

#[test]
fn re_consolidating_an_unchanged_repo_is_a_stable_no_op() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    build_history(&store, 8, 4);

    let first = store
        .consolidate(ConsolidateOptions::default())
        .expect("first consolidate");
    let objects_before = all_object_oids(&repo);
    let second = store
        .consolidate(ConsolidateOptions::default())
        .expect("second consolidate");

    // Consolidation is deterministic: an unchanged reachable set yields the
    // identical pack, so nothing churns and the object set is stable.
    assert_eq!(first.objects, second.objects);
    assert_eq!(objects_before, all_object_oids(&repo));
    assert_eq!(pack_count(&repo), 1, "no pack pile-up");
    git(&repo, &["fsck", "--strict"]);
}

#[test]
fn a_later_consolidation_supersedes_the_earlier_pack() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let mut all = build_history(&store, 8, 4);

    store
        .consolidate(ConsolidateOptions::default())
        .expect("first consolidate");
    assert_eq!(pack_count(&repo), 1);

    // Grow the repository, then consolidate again: the new pack covers the
    // old one's objects, so the earlier pack must be superseded rather than
    // left to pile up.
    let mut ref_current = store
        .read_ref("refs/heads/main")
        .expect("ref")
        .expect("present");
    let mut prev_data = make_chunk(0, 3);
    let mut prev_hash = store.put(&prev_data).expect("seed");
    all.push((prev_hash, prev_data.clone()));
    for v in 4..7 {
        prev_data = make_chunk(0, v);
        let hash = store.put(&prev_data).expect("put");
        store.record_base_hints(&[(hash, prev_hash)]).expect("hint");
        let mut new = NewCommit::new(b"grow\n", "s\n", "grow");
        let parents = [ref_current];
        new.parents = &parents;
        let anchors = [hash];
        new.anchors = &anchors;
        let commit = store.create_commit(&new).expect("commit");
        store
            .write_ref("refs/heads/main", Some(&ref_current), &commit)
            .expect("ref");
        ref_current = commit;
        all.push((hash, prev_data.clone()));
        prev_hash = hash;
    }

    let stats = store
        .consolidate(ConsolidateOptions::default())
        .expect("second consolidate");
    assert!(
        stats.pruned_packs >= 1,
        "the earlier pack must be superseded"
    );
    assert_eq!(pack_count(&repo), 1, "only the newest pack should remain");

    // Everything still reads back through a freshly opened store.
    let store = GitStore::open(&repo).expect("reopen");
    for (hash, data) in &all {
        assert_eq!(store.get(hash).unwrap().unwrap().as_ref(), data.as_slice());
    }
    git(&repo, &["fsck", "--strict"]);
}
