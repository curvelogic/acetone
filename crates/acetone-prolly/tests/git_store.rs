//! Integration against the real `GitStore` backend (ADR-0002): the prolly
//! layer must behave identically over git as over the in-memory store, and
//! the anchor-set export must satisfy the commit-anchoring contract end to
//! end (`git fsck`-connected commits).

mod common;

use acetone_prolly::{
    BatchOp, ChunkParams, apply_batch, bulk_load, diff, get, merge, reachable_chunks, scan,
    scan_rev,
};
use acetone_store::{CommitStore, GitStore, NewCommit, RefStore};
use common::{Map, MemStore, bulk_entries};

fn new_git_store() -> (tempfile::TempDir, GitStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = GitStore::create(&dir.path().join("repo.git")).expect("create store");
    (dir, store)
}

/// Run a git command in `repo`, panicking (with full output) on failure.
fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn full_lifecycle_over_git() {
    let (_dir, store) = new_git_store();
    let mut map = bulk_entries(3000, 0x917);
    let root = bulk_load(&store, ChunkParams::default(), map.clone()).expect("bulk_load");

    // Reads.
    let probe = map.keys().nth(1500).expect("key").clone();
    assert_eq!(
        get(&store, &root, &probe).expect("get").as_deref(),
        Some(map[&probe].as_slice())
    );
    let all: Vec<(Vec<u8>, Vec<u8>)> = scan(&store, &root, ..)
        .expect("scan")
        .map(|r| r.map(|(k, v)| (k.to_vec(), v.to_vec())))
        .collect::<Result<_, _>>()
        .expect("items");
    assert_eq!(all, map.clone().into_iter().collect::<Vec<_>>());
    let mut rev: Vec<Vec<u8>> = scan_rev(&store, &root, ..)
        .expect("scan_rev")
        .map(|r| r.map(|(k, _)| k.to_vec()))
        .collect::<Result<_, _>>()
        .expect("items");
    rev.reverse();
    assert_eq!(
        rev,
        map.keys().cloned().collect::<Vec<_>>(),
        "reverse scan over git"
    );

    // Batch + diff + merge, all over git.
    let k_ours = map.keys().nth(100).expect("key").clone();
    let k_theirs = map.keys().nth(2000).expect("key").clone();
    let ours = apply_batch(
        &store,
        &root,
        vec![BatchOp::Put(k_ours.clone(), b"ours".to_vec())],
    )
    .expect("ours");
    let theirs = apply_batch(
        &store,
        &root,
        vec![BatchOp::Put(k_theirs.clone(), b"theirs".to_vec())],
    )
    .expect("theirs");
    let d: Vec<_> = diff(&store, &root, &ours)
        .expect("diff")
        .collect::<Result<_, _>>()
        .expect("entries");
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].key.as_ref(), k_ours.as_slice());

    let outcome = merge(&store, &root, &ours, &theirs).expect("merge");
    assert!(outcome.conflicts.is_empty());
    map.insert(k_ours, b"ours".to_vec());
    map.insert(k_theirs, b"theirs".to_vec());
    let fresh = bulk_load(&store, ChunkParams::default(), map).expect("fresh");
    assert_eq!(outcome.root, fresh, "merge over git is history-independent");
}

#[test]
fn cross_store_determinism_memstore_vs_gitstore() {
    // The same contents must produce the same root hash whether the chunks
    // live in memory or in a git object database — nothing about the store
    // may leak into the tree (spec §3.2; ChunkStore contract).
    let (_dir, git_store) = new_git_store();
    let mem_store = MemStore::new();
    let map = bulk_entries(2500, 0x5107);

    let git_root = bulk_load(&git_store, ChunkParams::default(), map.clone()).expect("git");
    let mem_root = bulk_load(&mem_store, ChunkParams::default(), map.clone()).expect("mem");
    assert_eq!(
        git_root.hash(),
        mem_root.hash(),
        "root hashes diverged across backends"
    );
    assert_eq!(git_root.height(), mem_root.height());

    // And after a batch, still identical.
    let key = map.keys().next().expect("key").clone();
    let ops = vec![
        BatchOp::Put(key, b"update".to_vec()),
        BatchOp::Put(b"brand/new".to_vec(), b"value".to_vec()),
    ];
    let git_v2 = apply_batch(&git_store, &git_root, ops.clone()).expect("git batch");
    let mem_v2 = apply_batch(&mem_store, &mem_root, ops).expect("mem batch");
    assert_eq!(git_v2.hash(), mem_v2.hash());
}

#[test]
fn anchored_commit_is_fsck_connected_and_survives_gc() {
    let (dir, store) = new_git_store();
    let repo = dir.path().join("repo.git");
    let map: Map = bulk_entries(2000, 0x0a9c);
    let root = bulk_load(&store, ChunkParams::default(), map.clone()).expect("bulk_load");

    // The complete chunk set from the walk is exactly what the commit
    // needs to anchor.
    let anchors = reachable_chunks(&store, &root).expect("anchors");
    let manifest = format!("root: {}\nheight: {}\n", root.hash(), root.height());
    let mut commit = NewCommit::new(manifest.as_bytes(), "prolly test", "anchor test");
    commit.anchors = &anchors;
    let commit_id = store.create_commit(&commit).expect("create_commit");
    store
        .write_ref("refs/acetone/test", None, &commit_id)
        .expect("write_ref");

    // Fully connected under strict fsck…
    git(&repo, &["fsck", "--strict", "--no-dangling"]);
    // …and the map survives an aggressive gc.
    git(&repo, &["gc", "--prune=now", "--aggressive", "--quiet"]);
    let probe = map.keys().nth(1000).expect("key");
    assert_eq!(
        get(&store, &root, probe).expect("get after gc").as_deref(),
        Some(map[probe].as_slice()),
        "anchored chunks must survive git gc --prune=now"
    );
    let entries: Vec<_> = scan(&store, &root, ..)
        .expect("scan after gc")
        .collect::<Result<_, _>>()
        .expect("entries after gc");
    assert_eq!(entries.len(), map.len());
}

#[test]
fn unanchored_chunks_are_pruned_demonstrating_the_contract() {
    // The inverse of the test above: without anchors, gc destroys the map.
    // This pins the *reason* reachable_chunks must be complete.
    let (dir, store) = new_git_store();
    let repo = dir.path().join("repo.git");
    let map: Map = bulk_entries(2000, 0x901e);
    let root = bulk_load(&store, ChunkParams::default(), map).expect("bulk_load");

    git(&repo, &["gc", "--prune=now", "--quiet"]);
    assert!(
        get(&store, &root, b"bulk/000000000000901e/000000").is_err()
            || scan(&store, &root, ..)
                .and_then(|s| s.collect::<Result<Vec<_>, _>>())
                .is_err(),
        "unanchored chunks should have been pruned"
    );
}
