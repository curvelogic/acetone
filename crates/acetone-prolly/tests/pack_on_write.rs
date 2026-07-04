//! Pack-on-write predecessor recording end to end (ADR-0011, bead
//! acetone-63m.13).
//!
//! [`apply_batch_recording`] must (a) produce a tree byte-for-byte identical
//! to [`apply_batch`] — the recorder only observes — and (b) hand out
//! `(new_chunk, predecessor)` pairs that let `GitStore::consolidate` delta the
//! rewritten chunks against their real predecessors, recovering the retention
//! win while preserving every byte.

use std::collections::BTreeSet;

use acetone_prolly::{
    BatchOp, ChunkParams, apply_batch, apply_batch_recording, bulk_load, get, reachable_chunks,
};
use acetone_store::{CommitStore, ConsolidateOptions, GitStore, Hash, NewCommit, RefStore};

fn new_git_store() -> (tempfile::TempDir, GitStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = GitStore::create(&dir.path().join("repo.git")).expect("create store");
    (dir, store)
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn key(i: usize) -> Vec<u8> {
    format!("key{i:06}").into_bytes()
}

fn value(i: usize, version: usize) -> Vec<u8> {
    // Wide enough that a leaf holds tens of entries (so a churn touches some
    // leaves and leaves others untouched), and version-dependent so an update
    // genuinely changes bytes.
    format!("value-{i:06}-v{version}-{}", "x".repeat(40)).into_bytes()
}

type Entries = Vec<(Vec<u8>, Vec<u8>)>;

/// A base map of `n` keys plus a churn updating every `stride`-th key.
fn workload(n: usize, stride: usize) -> (Entries, Vec<BatchOp>) {
    let base: Vec<(Vec<u8>, Vec<u8>)> = (0..n).map(|i| (key(i), value(i, 0))).collect();
    let ops: Vec<BatchOp> = (0..n)
        .step_by(stride)
        .map(|i| BatchOp::Put(key(i), value(i, 1)))
        .collect();
    (base, ops)
}

#[test]
fn recording_produces_the_same_tree_as_apply_batch() {
    // Observer-only: the recorded rebuild and the plain rebuild must yield the
    // identical root over the same store.
    let (_dir, store) = new_git_store();
    let params = ChunkParams::default();
    let (base, ops) = workload(3000, 97);

    let root0 = bulk_load(&store, params, base).expect("bulk load");
    let plain = apply_batch(&store, &root0, ops.clone()).expect("apply_batch");
    let (recorded, pairs) = apply_batch_recording(&store, &root0, ops).expect("recording");

    assert_eq!(
        plain.hash(),
        recorded.hash(),
        "recording must not change bytes"
    );
    assert_eq!(plain.height(), recorded.height());
    assert!(!pairs.is_empty(), "a churn must record some predecessors");
}

#[test]
fn recorded_pairs_link_new_chunks_to_old_chunks() {
    let (_dir, store) = new_git_store();
    let params = ChunkParams::default();
    let (base, ops) = workload(3000, 97);

    let root0 = bulk_load(&store, params, base).expect("bulk load");
    let old_chunks: BTreeSet<Hash> = reachable_chunks(&store, &root0)
        .unwrap()
        .into_iter()
        .collect();
    let (root1, pairs) = apply_batch_recording(&store, &root0, ops).expect("recording");
    let new_chunks: BTreeSet<Hash> = reachable_chunks(&store, &root1)
        .unwrap()
        .into_iter()
        .collect();

    assert!(!pairs.is_empty());
    for (new, base) in &pairs {
        assert_ne!(new, base, "a chunk is never its own predecessor");
        assert!(
            new_chunks.contains(new),
            "new chunk {new} must be in the new tree"
        );
        assert!(
            old_chunks.contains(base),
            "predecessor {base} must be an old chunk"
        );
    }
}

#[test]
fn recorded_predecessors_drive_consolidation_deltas() {
    let (dir, store) = new_git_store();
    let repo = dir.path().join("repo.git");
    let params = ChunkParams::default();
    let (base, ops) = workload(4000, 40);
    let churn_keys: Vec<usize> = (0..4000).step_by(40).collect();

    // Version 0, anchored in a commit so its chunks are reachable.
    let root0 = bulk_load(&store, params, base).expect("bulk load");
    let anchors0 = reachable_chunks(&store, &root0).expect("anchors0");
    let commit0 = {
        let mut nc = NewCommit::new(b"v0\n", "v0\n", "v0");
        nc.anchors = &anchors0;
        store.create_commit(&nc).expect("commit0")
    };
    store
        .write_ref("refs/heads/main", None, &commit0)
        .expect("ref0");

    // Version 1: churn with recording, anchor, and hand the predecessors to
    // the store for consolidation.
    let (root1, pairs) = apply_batch_recording(&store, &root0, ops).expect("recording");
    store.record_base_hints(&pairs).expect("record hints");
    let anchors1 = reachable_chunks(&store, &root1).expect("anchors1");
    let commit1 = {
        let mut nc = NewCommit::new(b"v1\n", "v1\n", "v1");
        let parents = [commit0];
        nc.parents = &parents;
        nc.anchors = &anchors1;
        store.create_commit(&nc).expect("commit1")
    };
    store
        .write_ref("refs/heads/main", Some(&commit0), &commit1)
        .expect("ref1");

    let stats = store
        .consolidate(ConsolidateOptions::default())
        .expect("consolidate");
    assert!(
        stats.deltas > 0,
        "recorded predecessors must yield deltas (got {} of {})",
        stats.deltas,
        stats.objects
    );

    // Representation-only: both versions still read back correctly through a
    // freshly opened store.
    let store = GitStore::open(&repo).expect("reopen");
    for i in (0..4000).step_by(211) {
        let expected0 = value(i, 0);
        assert_eq!(
            get(&store, &root0, &key(i)).unwrap().unwrap().as_ref(),
            expected0.as_slice(),
            "v0 key {i}"
        );
        let expected1 = if churn_keys.contains(&i) {
            value(i, 1)
        } else {
            value(i, 0)
        };
        assert_eq!(
            get(&store, &root1, &key(i)).unwrap().unwrap().as_ref(),
            expected1.as_slice(),
            "v1 key {i}"
        );
    }
    git(&repo, &["fsck", "--strict"]);
}
