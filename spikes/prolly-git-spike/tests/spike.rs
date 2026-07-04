//! Integration tests for the prolly-git spike: round-trip, scan order,
//! history independence, and git interoperability (CLI readability,
//! gc/clone durability).

use std::path::Path;
use std::process::Command;

use prolly_git_spike::{BatchOp, Store};

fn new_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::create(&dir.path().join("repo.git")).expect("create store");
    (dir, store)
}

/// Deterministic pseudo-random permutation (Fisher-Yates over an LCG).
fn shuffled<T>(mut v: Vec<T>, seed: u64) -> Vec<T> {
    let mut s = seed
        .wrapping_mul(2862933555777941757)
        .wrapping_add(3037000493);
    for i in (1..v.len()).rev() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = ((s >> 33) as usize) % (i + 1);
        v.swap(i, j);
    }
    v
}

fn kv(i: usize) -> (Vec<u8>, Vec<u8>) {
    let key = format!("asset:{i:08}").into_bytes();
    let value = format!(
        "value-{i}-{:016x}-padding-padding-padding-padding",
        (i as u64).wrapping_mul(0x9e3779b97f4a7c15)
    )
    .into_bytes();
    (key, value)
}

fn entries(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n).map(kv).collect()
}

#[test]
fn round_trip_point_reads() {
    let (_dir, store) = new_store();
    let data = entries(5000);
    let root = store.bulk_load(data.clone()).expect("bulk_load");
    assert!(root.height >= 2, "5000 entries should not fit one leaf");
    for (k, v) in &data {
        assert_eq!(store.get(&root, k).expect("get").as_ref(), Some(v));
    }
    assert_eq!(store.get(&root, b"asset:99999999").expect("get"), None);
    assert_eq!(store.get(&root, b"").expect("get"), None);
}

#[test]
fn scan_is_ordered_and_respects_ranges() {
    let (_dir, store) = new_store();
    let data = entries(3000);
    // Load shuffled; scans must come back sorted.
    let root = store
        .bulk_load(shuffled(data.clone(), 7))
        .expect("bulk_load");

    let all: Vec<_> = store
        .range_scan(&root, ..)
        .expect("scan")
        .collect::<Result<_, _>>()
        .expect("scan items");
    assert_eq!(all, data, "full scan must equal sorted contents");

    let (lo, hi) = (kv(100).0, kv(200).0);
    let sub: Vec<_> = store
        .range_scan(&root, lo.clone()..hi.clone())
        .expect("scan")
        .collect::<Result<_, _>>()
        .expect("scan items");
    assert_eq!(sub, data[100..200].to_vec(), "half-open subrange");

    use std::ops::Bound;
    let sub_incl: Vec<_> = store
        .range_scan(&root, (Bound::Excluded(lo), Bound::Included(hi)))
        .expect("scan")
        .collect::<Result<_, _>>()
        .expect("scan items");
    assert_eq!(sub_incl, data[101..=200].to_vec(), "excl/incl subrange");

    let empty: Vec<_> = store
        .range_scan(&root, kv(9000).0..)
        .expect("scan")
        .collect::<Result<_, _>>()
        .expect("scan items");
    assert!(empty.is_empty(), "range past the end is empty");
}

/// The determinism smoke test the task demands: shuffled bulk-load orders
/// must produce identical root OIDs.
#[test]
fn history_independence_shuffled_bulk_loads() {
    let (_dir, store) = new_store();
    let data = entries(4000);
    let r1 = store.bulk_load(data.clone()).expect("bulk_load");
    let r2 = store
        .bulk_load(shuffled(data.clone(), 1))
        .expect("bulk_load");
    let r3 = store.bulk_load(shuffled(data, 2)).expect("bulk_load");
    assert_eq!(r1, r2);
    assert_eq!(r1, r3);
}

/// Stronger form: different operation histories converging on the same
/// contents must produce the same root as a fresh bulk load.
#[test]
fn history_independence_across_operation_histories() {
    let (_dir, store) = new_store();
    let final_contents = entries(4000);
    let reference = store.bulk_load(final_contents.clone()).expect("bulk_load");

    // Path B: load 60%, then apply the rest as three shuffled Put batches.
    let (first, rest) = final_contents.split_at(2400);
    let mut root_b = store.bulk_load(first.to_vec()).expect("bulk_load");
    let rest_shuffled = shuffled(rest.to_vec(), 3);
    for batch in rest_shuffled.chunks(541) {
        let ops = batch
            .iter()
            .map(|(k, v)| BatchOp::Put(k.clone(), v.clone()));
        root_b = store.apply_batch(&root_b, ops).expect("apply_batch");
    }
    assert_eq!(reference, root_b, "bulk-then-batches must converge");

    // Path C: load a superset, then delete the extras (shuffled order).
    let mut superset = final_contents.clone();
    superset.extend((10_000..10_500).map(kv));
    let mut root_c = store.bulk_load(superset).expect("bulk_load");
    let extras: Vec<Vec<u8>> = shuffled((10_000..10_500).map(|i| kv(i).0).collect(), 4);
    for batch in extras.chunks(97) {
        let ops = batch.iter().map(|k| BatchOp::Delete(k.clone()));
        root_c = store.apply_batch(&root_c, ops).expect("apply_batch");
    }
    assert_eq!(reference, root_c, "superset-then-delete must converge");

    // Path D: load with wrong values, then overwrite with the right ones.
    let wrong: Vec<_> = final_contents
        .iter()
        .map(|(k, _)| (k.clone(), b"wrong".to_vec()))
        .collect();
    let mut root_d = store.bulk_load(wrong).expect("bulk_load");
    for batch in shuffled(final_contents.clone(), 5).chunks(1013) {
        let ops = batch
            .iter()
            .map(|(k, v)| BatchOp::Put(k.clone(), v.clone()));
        root_d = store.apply_batch(&root_d, ops).expect("apply_batch");
    }
    assert_eq!(reference, root_d, "overwrite path must converge");

    // Mutate then revert: must return to the exact reference root.
    let (k0, v0) = kv(123);
    let mutated = store
        .apply_batch(&reference, vec![BatchOp::Put(k0.clone(), b"temp".to_vec())])
        .expect("apply_batch");
    assert_ne!(reference, mutated);
    let reverted = store
        .apply_batch(&mutated, vec![BatchOp::Put(k0, v0)])
        .expect("apply_batch");
    assert_eq!(reference, reverted, "revert must restore the exact root");
}

#[test]
fn empty_map_and_edge_cases() {
    let (_dir, store) = new_store();
    let empty = store.bulk_load(Vec::new()).expect("bulk_load empty");
    assert_eq!(empty.height, 1);
    assert_eq!(store.get(&empty, b"anything").expect("get"), None);
    let scanned: Vec<_> = store
        .range_scan(&empty, ..)
        .expect("scan")
        .collect::<Result<_, _>>()
        .expect("scan items");
    assert!(scanned.is_empty());

    // Insert into empty, then delete everything: back to the empty root.
    let data = entries(50);
    let loaded = store
        .apply_batch(
            &empty,
            data.iter().map(|(k, v)| BatchOp::Put(k.clone(), v.clone())),
        )
        .expect("apply_batch");
    assert_eq!(loaded, store.bulk_load(data.clone()).expect("bulk_load"));
    let emptied = store
        .apply_batch(
            &loaded,
            data.iter().map(|(k, _)| BatchOp::Delete(k.clone())),
        )
        .expect("apply_batch");
    assert_eq!(emptied, empty, "deleting all keys restores the empty root");

    // Duplicate keys in one batch: last op wins.
    let dup = store
        .apply_batch(
            &empty,
            vec![
                BatchOp::Put(b"k".to_vec(), b"first".to_vec()),
                BatchOp::Put(b"k".to_vec(), b"second".to_vec()),
            ],
        )
        .expect("apply_batch");
    assert_eq!(
        store.get(&dup, b"k").expect("get"),
        Some(b"second".to_vec())
    );
}

/// The incremental path must genuinely reuse unchanged chunks, not
/// silently degenerate into a full rebuild: a single-key update on a
/// multi-level tree may rewrite only a handful of chunks.
#[test]
fn single_key_update_writes_few_chunks() {
    let (_dir, store) = new_store();
    let root = store.bulk_load(entries(20_000)).expect("bulk_load");
    assert!(root.height >= 3, "want a tree with internal levels");
    let full_build = store.chunks_written();
    assert!(full_build > 100, "expected many chunks, got {full_build}");

    let before = store.chunks_written();
    let (k, _) = kv(10_000);
    store
        .apply_batch(&root, vec![BatchOp::Put(k, b"updated".to_vec())])
        .expect("apply_batch");
    let delta = store.chunks_written() - before;
    // Path rewrite: ~1-2 chunks per level plus splice slack.
    let budget = 4 * root.height as u64 + 4;
    assert!(
        delta <= budget,
        "single-key update wrote {delta} chunks (budget {budget}) — chunk reuse is broken"
    );
}

#[test]
fn manifest_commit_round_trip_and_parent_chaining() {
    let (_dir, store) = new_store();
    let root1 = store.bulk_load(entries(1000)).expect("bulk_load");
    let ref_name = "refs/spike/run-1";
    let c1 = store
        .commit_root(&root1, ref_name, "import v1")
        .expect("commit");
    assert_eq!(store.read_manifest(ref_name).expect("read_manifest"), root1);

    let root2 = store
        .apply_batch(
            &root1,
            vec![BatchOp::Put(b"new-key".to_vec(), b"v".to_vec())],
        )
        .expect("apply_batch");
    let c2 = store
        .commit_root(&root2, ref_name, "import v2")
        .expect("commit");
    assert_ne!(c1, c2);
    assert_eq!(store.read_manifest(ref_name).expect("read_manifest"), root2);
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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

/// The Gate A questions: is the commit readable by stock git, does the data
/// survive `git gc --prune=now`, and does a clone carry it?
#[test]
fn git_cli_readability_gc_and_clone_durability() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo.git");
    let store = Store::create(&repo).expect("create store");
    let data = entries(2000);
    let root = store.bulk_load(data.clone()).expect("bulk_load");
    let ref_name = "refs/spike/run-1";
    let commit_oid = store
        .commit_root(&root, ref_name, "spike import")
        .expect("commit");
    drop(store);

    // Plain git CLI can read the history and the manifest.
    let log = git(&repo, &["log", "--format=%H %s", ref_name]);
    assert!(log.contains(&commit_oid.to_string()));
    assert!(log.contains("spike import"));
    let manifest = git(&repo, &["cat-file", "-p", &format!("{ref_name}:manifest")]);
    assert!(manifest.contains(&format!("root: {}", root.oid)));
    let cat = git(&repo, &["cat-file", "-t", &root.oid.to_string()]);
    assert_eq!(cat.trim(), "blob");

    // fsck: connected, nothing missing or corrupt.
    git(&repo, &["fsck", "--strict"]);

    // gc --prune=now: every chunk is reachable via the chunks/ tree, so the
    // committed version must survive aggressive pruning.
    git(&repo, &["gc", "--prune=now", "--quiet"]);
    let store = Store::open(&repo).expect("reopen after gc");
    let reread = store.read_manifest(ref_name).expect("read_manifest");
    assert_eq!(reread, root);
    for (k, v) in data.iter().step_by(97) {
        assert_eq!(store.get(&reread, k).expect("get").as_ref(), Some(v));
    }

    // A mirror clone carries the spike ref and all data. (Note for Gate A:
    // a *plain* clone only transfers refs/heads and refs/tags, so real
    // acetone refs must live in a fetched namespace or configure refspecs.)
    let clone_path = dir.path().join("clone.git");
    git(
        dir.path(),
        &[
            "clone",
            "--mirror",
            "--quiet",
            repo.to_str().unwrap(),
            clone_path.to_str().unwrap(),
        ],
    );
    let cloned = Store::open(&clone_path).expect("open clone");
    let cloned_root = cloned.read_manifest(ref_name).expect("read_manifest");
    assert_eq!(cloned_root, root);
    for (k, v) in data.iter().step_by(211) {
        assert_eq!(cloned.get(&cloned_root, k).expect("get").as_ref(), Some(v));
    }
    let scanned: Vec<_> = cloned
        .range_scan(&cloned_root, ..)
        .expect("scan")
        .collect::<Result<_, _>>()
        .expect("scan items");
    assert_eq!(scanned, data);
}
