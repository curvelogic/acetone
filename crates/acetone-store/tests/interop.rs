//! Interoperability with stock git, verified by shelling out to the real
//! git CLI (library code never shells out; tests MAY, to prove interop):
//! fsck-clean object graphs, `git log` readability, trailer visibility,
//! gc survival of everything reachable from refs, and clone round-trips.

mod common;

use acetone_store::{ChunkStore, CommitStore, GitStore, NewCommit, RefStore, Signature};
use common::{git, new_store, repo_path};

const REF: &str = "refs/acetone/branches/main";

/// One committed version: manifest + summary + trailers, anchored to REF.
fn seed_commit(store: &GitStore) -> (Vec<u8>, acetone_store::Hash) {
    let manifest = b"format: acetone-v0\nnodes: cafebabe\n".to_vec();
    let trailers = vec![
        ("Acetone-Source".to_owned(), "unit-test".to_owned()),
        (
            "Acetone-Extractor".to_owned(),
            "interop-test 0.1".to_owned(),
        ),
    ];
    let id = store
        .create_commit(&NewCommit {
            manifest: &manifest,
            summary: "# acetone graph\n\nSeeded by the interop test.\n",
            message: "import: seed data",
            trailers: &trailers,
            parents: &[],
            author: Signature::default(),
        })
        .expect("create_commit");
    store.write_ref(REF, None, &id).expect("write_ref");
    (manifest, id)
}

#[test]
fn git_fsck_is_clean_and_log_shows_the_commit() {
    let (dir, store) = new_store();
    let (manifest, id) = seed_commit(&store);
    drop(store);
    let repo = repo_path(&dir);

    // The whole object graph is valid and connected under strict fsck.
    git(&repo, &["fsck", "--strict"]);

    // Stock git shows the commit on our ref, with subject and trailers.
    let log = git(&repo, &["log", "--format=%H %s", REF]);
    assert!(log.contains(&id.to_hex()), "log: {log}");
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

    // The manifest and summary are plain files in the commit tree.
    let manifest_out = git(&repo, &["cat-file", "-p", &format!("{REF}:manifest")]);
    assert_eq!(manifest_out.as_bytes(), manifest.as_slice());
    let summary_out = git(&repo, &["cat-file", "-p", &format!("{REF}:README.md")]);
    assert!(summary_out.contains("acetone graph"));
}

#[test]
fn everything_reachable_from_refs_survives_gc() {
    let (dir, store) = new_store();
    let (manifest, id) = seed_commit(&store);

    // An un-anchored chunk: durable now, but reachable from nothing.
    let orphan = store.put(b"unreferenced chunk").expect("put");
    assert!(store.get(&orphan).expect("get").is_some());
    drop(store);

    let repo = repo_path(&dir);
    git(&repo, &["gc", "--prune=now", "--quiet"]);

    let store = GitStore::open(&repo).expect("reopen after gc");
    // The commit and its manifest are anchored by the ref: they survive.
    assert_eq!(store.read_ref(REF).expect("read_ref"), Some(id));
    let commit = store
        .read_commit(&id)
        .expect("read_commit")
        .expect("present");
    assert_eq!(commit.manifest.as_ref(), manifest.as_slice());

    // The orphan chunk was pruned: this is the documented durability
    // model — chunks must be anchored to a commit to survive gc, which is
    // the layer above's responsibility (bead acetone-63m.10).
    assert!(
        store.get(&orphan).expect("get").is_none(),
        "unanchored chunk should have been pruned"
    );
}

#[test]
fn mirror_clone_round_trips() {
    let (dir, store) = new_store();
    let (manifest, id) = seed_commit(&store);
    drop(store);
    let repo = repo_path(&dir);

    // Note (spec §3.5 / ADR-0002): a *plain* clone transfers only
    // refs/heads and refs/tags; acetone refs need a mirror clone or an
    // explicit refspec.
    let clone_path = dir.path().join("clone.git");
    git(
        dir.path(),
        &[
            "clone",
            "--mirror",
            "--quiet",
            repo.to_str().expect("utf8 path"),
            clone_path.to_str().expect("utf8 path"),
        ],
    );

    // The clone is of untrusted origin: GitStore::open applies the
    // reduced-trust posture and everything still reads back.
    let cloned = GitStore::open(&clone_path).expect("open clone");
    assert_eq!(cloned.read_ref(REF).expect("read_ref"), Some(id));
    let commit = cloned
        .read_commit(&id)
        .expect("read_commit")
        .expect("present");
    assert_eq!(commit.manifest.as_ref(), manifest.as_slice());
    assert_eq!(commit.trailers[0].0, "Acetone-Source");

    // And the clone itself is fsck-clean.
    git(&clone_path, &["fsck", "--strict"]);
}

#[test]
fn hostile_repo_local_config_is_not_honoured() {
    // Plant config a hostile clone could carry. Reduced-trust opening must
    // ignore trust-sensitive values; the repository must still open and
    // serve objects. (What gix disables is documented on GitStore.)
    let (dir, store) = new_store();
    let (manifest, id) = seed_commit(&store);
    drop(store);
    let repo = repo_path(&dir);
    git(&repo, &["config", "core.fsmonitor", "/tmp/evil-hook"]);
    git(&repo, &["config", "core.sshCommand", "/tmp/evil-ssh"]);
    git(&repo, &["config", "core.hooksPath", "/tmp/evil-hooks"]);

    let store = GitStore::open(&repo).expect("open with hostile config");
    let commit = store
        .read_commit(&id)
        .expect("read_commit")
        .expect("present");
    assert_eq!(commit.manifest.as_ref(), manifest.as_slice());
}
