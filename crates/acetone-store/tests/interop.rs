//! Interoperability with stock git, verified by shelling out to the real
//! git CLI (library code never shells out; tests MAY, to prove interop):
//! fsck-clean object graphs, `git log` readability, trailer visibility,
//! gc survival and clone transfer of anchored chunks, and SHA-256
//! repositories.

mod common;

use acetone_store::{
    ChunkStore, CommitStore, GitStore, GitStoreOptions, Hash, NewCommit, ObjectFormat, RefStore,
};
use common::{git, new_store, repo_path};

const REF: &str = "refs/acetone/branches/main";

/// One committed version anchored to REF: chunks + manifest + summary +
/// trailers, with every chunk in the anchor list (the documented contract).
struct Seeded {
    manifest: Vec<u8>,
    chunks: Vec<(Hash, Vec<u8>)>,
    commit: Hash,
}

fn seed_commit(store: &GitStore) -> Seeded {
    let chunks: Vec<(Hash, Vec<u8>)> = (0..200u32)
        .map(|i| {
            let data = format!("chunk-{i}-payload").into_bytes();
            (store.put(&data).expect("put chunk"), data)
        })
        .collect();
    // The manifest references chunks by content — invisible to git, which
    // is exactly why the anchors list below must carry all of them.
    let manifest = format!(
        "format: acetone-v0\nroot: {}\nchunks: {}\n",
        chunks[0].0,
        chunks.len()
    )
    .into_bytes();
    let anchors: Vec<Hash> = chunks.iter().map(|(hash, _)| *hash).collect();
    let trailers = vec![
        ("Acetone-Source".to_owned(), "unit-test".to_owned()),
        (
            "Acetone-Extractor".to_owned(),
            "interop-test 0.1".to_owned(),
        ),
    ];
    let mut new = NewCommit::new(
        &manifest,
        "# acetone graph\n\nSeeded by the interop test.\n",
        "import: seed data",
    );
    new.trailers = &trailers;
    new.anchors = &anchors;
    let commit = store.create_commit(&new).expect("create_commit");
    store.write_ref(REF, None, &commit).expect("write_ref");
    Seeded {
        manifest,
        chunks,
        commit,
    }
}

#[test]
fn git_fsck_is_clean_and_log_shows_the_commit() {
    let (dir, store) = new_store();
    let seeded = seed_commit(&store);
    drop(store);
    let repo = repo_path(&dir);

    // The whole object graph — commit, manifest, summary, anchored chunks
    // — is valid and connected under strict fsck.
    git(&repo, &["fsck", "--strict"]);

    // Stock git shows the commit on our ref, with subject and trailers.
    let log = git(&repo, &["log", "--format=%H %s", REF]);
    assert!(log.contains(&seeded.commit.to_hex()), "log: {log}");
    assert!(log.contains("import: seed data"), "log: {log}");

    let trailer_out = git(
        &repo,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Acetone-Source,valueonly)",
            REF,
        ],
    );
    assert_eq!(trailer_out.trim(), "unit-test");

    // The manifest and summary are plain files in the commit tree, and the
    // anchored chunks are visible as tree entries.
    let manifest_out = git(&repo, &["cat-file", "-p", &format!("{REF}:manifest")]);
    assert_eq!(manifest_out.as_bytes(), seeded.manifest.as_slice());
    let summary_out = git(&repo, &["cat-file", "-p", &format!("{REF}:README.md")]);
    assert!(summary_out.contains("acetone graph"));
    let (first_chunk, first_data) = &seeded.chunks[0];
    let hex = first_chunk.to_hex();
    let chunk_out = git(
        &repo,
        &[
            "cat-file",
            "-p",
            &format!("{REF}:chunks/{}/{}", &hex[..2], &hex[2..]),
        ],
    );
    assert_eq!(chunk_out.as_bytes(), first_data.as_slice());
}

#[test]
fn anchored_chunks_survive_gc_and_unanchored_ones_are_pruned() {
    let (dir, store) = new_store();
    let seeded = seed_commit(&store);

    // An un-anchored chunk: durable now, but reachable from nothing.
    let orphan = store.put(b"unreferenced chunk").expect("put");
    assert!(store.get(&orphan).expect("get").is_some());
    drop(store);

    let repo = repo_path(&dir);
    git(&repo, &["gc", "--prune=now", "--quiet"]);

    let store = GitStore::open(&repo).expect("reopen after gc");
    // The commit, its manifest and every anchored chunk are reachable from
    // the ref: all survive.
    assert_eq!(store.read_ref(REF).expect("read_ref"), Some(seeded.commit));
    let commit = store
        .read_commit(&seeded.commit)
        .expect("read_commit")
        .expect("present");
    assert_eq!(commit.manifest.as_ref(), seeded.manifest.as_slice());
    for (hash, data) in &seeded.chunks {
        assert_eq!(
            store
                .get(hash)
                .expect("get anchored chunk")
                .expect("anchored chunk survives gc")
                .as_ref(),
            data.as_slice()
        );
    }

    // The orphan chunk was pruned: this is the documented durability
    // model — a chunk must be in some commit's anchors to survive gc.
    assert!(
        store.get(&orphan).expect("get").is_none(),
        "unanchored chunk should have been pruned"
    );
}

#[test]
fn mirror_clone_transfers_anchored_chunks_but_not_unanchored_ones() {
    let (dir, store) = new_store();
    let seeded = seed_commit(&store);
    // An un-anchored chunk: present in the origin's object database…
    let orphan = store.put(b"unreferenced chunk").expect("put");
    drop(store);
    let repo = repo_path(&dir);

    // Note (spec §3.5 / ADR-0002): a *plain* clone transfers only
    // refs/heads and refs/tags; acetone refs need a mirror clone or an
    // explicit refspec. `--no-local` forces the real transport path — a
    // same-filesystem clone would otherwise hardlink the whole objects
    // directory and hide what transfer actually moves.
    let clone_path = dir.path().join("clone.git");
    git(
        dir.path(),
        &[
            "clone",
            "--mirror",
            "--no-local",
            "--quiet",
            repo.to_str().expect("utf8 path"),
            clone_path.to_str().expect("utf8 path"),
        ],
    );

    // The clone is of untrusted origin: GitStore::open applies the
    // reduced-trust posture and everything still reads back.
    let cloned = GitStore::open(&clone_path).expect("open clone");
    assert_eq!(cloned.read_ref(REF).expect("read_ref"), Some(seeded.commit));
    let commit = cloned
        .read_commit(&seeded.commit)
        .expect("read_commit")
        .expect("present");
    assert_eq!(commit.manifest.as_ref(), seeded.manifest.as_slice());
    assert_eq!(commit.trailers[0].0, "Acetone-Source");

    // Every anchored chunk arrived intact…
    for (hash, data) in &seeded.chunks {
        assert_eq!(
            cloned
                .get(hash)
                .expect("get anchored chunk")
                .expect("anchored chunk transferred by clone")
                .as_ref(),
            data.as_slice()
        );
    }
    // …and the unanchored chunk did NOT transfer: git moves only
    // ref-reachable objects. This is why NewCommit::anchors must list the
    // complete chunk set — manifest content alone transfers nothing.
    assert!(
        cloned.get(&orphan).expect("get").is_none(),
        "unanchored chunk must not be transferred by clone"
    );

    // And the clone itself is fsck-clean.
    git(&clone_path, &["fsck", "--strict"]);
}

#[test]
fn sha256_store_round_trips_and_interops_with_git() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo.git");
    let mut options = GitStoreOptions::default();
    options.object_format = ObjectFormat::Sha256;
    let store = GitStore::create_with(&repo, options).expect("create sha256 store");

    // Hashes are 32 bytes wide and opaque to callers.
    let chunk = store.put(b"sha256 chunk").expect("put");
    assert_eq!(chunk.as_bytes().len(), 32);
    assert_eq!(
        store.get(&chunk).expect("get").expect("present").as_ref(),
        b"sha256 chunk"
    );

    // Full commit + ref round trip.
    let anchors = [chunk];
    let mut new = NewCommit::new(b"sha256 manifest", "summary", "sha256 commit");
    new.anchors = &anchors;
    let commit = store.create_commit(&new).expect("create_commit");
    assert_eq!(commit.as_bytes().len(), 32);
    store.write_ref(REF, None, &commit).expect("write_ref");
    let read = store
        .read_commit(&commit)
        .expect("read_commit")
        .expect("present");
    assert_eq!(read.manifest.as_ref(), b"sha256 manifest");

    // A reduced-trust reopen sees the same objects (object format is read
    // from the repository, not from options).
    drop(store);
    let reopened = GitStore::open(&repo).expect("reopen");
    assert_eq!(reopened.read_ref(REF).expect("read_ref"), Some(commit));

    // Stock git agrees this is a sha256 repository and can read it all.
    let format = git(&repo, &["rev-parse", "--show-object-format"]);
    assert_eq!(format.trim(), "sha256");
    git(&repo, &["fsck", "--strict"]);
    let log = git(&repo, &["log", "--format=%H %s", REF]);
    assert!(log.contains(&commit.to_hex()), "log: {log}");
    let manifest_out = git(&repo, &["cat-file", "-p", &format!("{REF}:manifest")]);
    assert_eq!(manifest_out.as_bytes(), b"sha256 manifest");
}

#[test]
fn hostile_repo_local_config_is_not_honoured() {
    // Plant config a hostile clone could carry. Reduced-trust opening must
    // ignore trust-sensitive values; the repository must still open and
    // serve objects. (What gix disables is documented on GitStore.)
    let (dir, store) = new_store();
    let seeded = seed_commit(&store);
    drop(store);
    let repo = repo_path(&dir);
    git(&repo, &["config", "core.fsmonitor", "/tmp/evil-hook"]);
    git(&repo, &["config", "core.sshCommand", "/tmp/evil-ssh"]);
    git(&repo, &["config", "core.hooksPath", "/tmp/evil-hooks"]);

    let store = GitStore::open(&repo).expect("open with hostile config");
    let commit = store
        .read_commit(&seeded.commit)
        .expect("read_commit")
        .expect("present");
    assert_eq!(commit.manifest.as_ref(), seeded.manifest.as_slice());
}
