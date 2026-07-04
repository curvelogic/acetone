//! CommitStore contract tests: create/read round trip including trailers,
//! parent chains and chunk anchors, input validation, and corrupt-object
//! handling (every decode failure must be a typed error, never a panic).

mod common;

use acetone_store::{ChunkStore, CommitStore, GitStore, Hash, NewCommit, Signature, StoreError};
use common::{git, git_stdin, new_capped_store, new_store, repo_path};

fn trailer(token: &str, value: &str) -> (String, String) {
    (token.to_owned(), value.to_owned())
}

#[test]
fn commit_round_trip_with_trailers() {
    let (_dir, store) = new_store();
    let manifest = b"format: acetone-v0\nnodes: 0000\n".as_slice();
    let trailers = vec![
        trailer("Acetone-Source", "s3://bucket/dump.csv"),
        trailer("Acetone-Extractor", "csv-import 1.2.0"),
        trailer("Acetone-Source-Hash", "deadbeef"),
    ];
    let mut new = NewCommit::new(
        manifest,
        "# Import\n\n42 nodes.\n",
        "import: initial load\n\nLoaded the initial dataset.",
    );
    new.trailers = &trailers;
    let id = store.create_commit(&new).expect("create_commit");

    let commit = store
        .read_commit(&id)
        .expect("read_commit")
        .expect("present");
    assert_eq!(commit.id, id);
    assert_eq!(commit.manifest.as_ref(), manifest);
    assert!(commit.parents.is_empty());
    assert!(commit.message.starts_with("import: initial load"));
    assert!(commit.message.contains("Loaded the initial dataset."));
    assert_eq!(commit.trailers, trailers);
}

#[test]
fn commit_round_trip_without_trailers() {
    let (_dir, store) = new_store();
    let id = store
        .create_commit(&NewCommit::new(b"m", "s", "plain commit"))
        .expect("create_commit");
    let commit = store.read_commit(&id).expect("read").expect("present");
    assert_eq!(commit.message.trim_end(), "plain commit");
    assert!(commit.trailers.is_empty());
}

#[test]
fn parent_chain_round_trips() {
    let (_dir, store) = new_store();
    let commit_with = |message, parents: &[Hash]| {
        let mut new = NewCommit::new(b"m", "s", message);
        new.parents = parents;
        store.create_commit(&new)
    };
    let c1 = commit_with("one", &[]).expect("c1");
    let c2 = commit_with("two", &[c1]).expect("c2");
    let c3 = commit_with("merge", &[c2, c1]).expect("c3");

    assert_eq!(
        store
            .read_commit(&c2)
            .expect("read")
            .expect("present")
            .parents,
        vec![c1]
    );
    assert_eq!(
        store
            .read_commit(&c3)
            .expect("read")
            .expect("present")
            .parents,
        vec![c2, c1],
        "parent order must be preserved"
    );
}

#[test]
fn anchored_commit_round_trips_and_reads_ignore_the_anchor_tree() {
    let (_dir, store) = new_store();
    let chunks: Vec<Hash> = (0..300u32)
        .map(|i| store.put(format!("chunk-{i}").as_bytes()).expect("put"))
        .collect();
    let manifest = b"manifest referencing chunks by content".as_slice();
    let mut new = NewCommit::new(manifest, "summary", "anchored commit");
    new.anchors = &chunks;
    let id = store.create_commit(&new).expect("create_commit");

    // read_commit returns exactly the same view as for an unanchored
    // commit: the chunks/ tree is a reachability artefact, not payload.
    let commit = store.read_commit(&id).expect("read").expect("present");
    assert_eq!(commit.manifest.as_ref(), manifest);
    assert!(commit.parents.is_empty());
}

#[test]
fn anchors_are_deduplicated_and_order_insensitive() {
    // The anchor tree is derived data: the same anchor *set* must produce
    // the same commit tree regardless of input order or duplicates.
    let (dir, store) = new_store();
    let chunks: Vec<Hash> = (0..50u32)
        .map(|i| store.put(format!("chunk-{i}").as_bytes()).expect("put"))
        .collect();
    let mut reversed_with_dups: Vec<Hash> = chunks.iter().rev().copied().collect();
    reversed_with_dups.extend_from_slice(&chunks[..10]);

    let commit_with = |anchors: &[Hash]| {
        let mut new = NewCommit::new(b"m", "s", "anchored");
        new.anchors = anchors;
        store.create_commit(&new).expect("create_commit")
    };
    let a = commit_with(&chunks);
    let b = commit_with(&reversed_with_dups);
    // Commits may differ (timestamps), but their trees must be identical.
    let tree_a = git(&repo_path(&dir), &["rev-parse", &format!("{a}^{{tree}}")]);
    let tree_b = git(&repo_path(&dir), &["rev-parse", &format!("{b}^{{tree}}")]);
    assert_eq!(tree_a, tree_b);
}

#[test]
fn anchoring_a_missing_object_is_rejected() {
    let (_dir, store) = new_store();
    let absent = Hash::from_hex("0123456789abcdef0123456789abcdef01234567").expect("hash");
    let anchors = [absent];
    let mut new = NewCommit::new(b"m", "s", "msg");
    new.anchors = &anchors;
    match store.create_commit(&new) {
        Err(StoreError::InvalidAnchor { hash, .. }) => assert_eq!(hash, absent),
        other => panic!("expected InvalidAnchor, got {other:?}"),
    }
}

#[test]
fn anchoring_a_non_blob_is_rejected() {
    let (dir, store) = new_store();
    let tree_hex = git_stdin(&repo_path(&dir), &["mktree"], b"");
    let tree_hash = Hash::from_hex(tree_hex.trim()).expect("hash");
    let anchors = [tree_hash];
    let mut new = NewCommit::new(b"m", "s", "msg");
    new.anchors = &anchors;
    match store.create_commit(&new) {
        Err(StoreError::InvalidAnchor { hash, .. }) => assert_eq!(hash, tree_hash),
        other => panic!("expected InvalidAnchor, got {other:?}"),
    }
}

#[test]
fn create_commit_is_idempotent_apart_from_time() {
    // Two commits with identical inputs may differ only through the
    // timestamp; the manifest blob and tree they share must be identical
    // objects (content addressing all the way down).
    let (_dir, store) = new_store();
    let new = NewCommit::new(b"same manifest", "same summary", "same message");
    let c1 = store.create_commit(&new).expect("c1");
    let c2 = store.create_commit(&new).expect("c2");
    let m1 = store
        .read_commit(&c1)
        .expect("read")
        .expect("present")
        .manifest;
    let m2 = store
        .read_commit(&c2)
        .expect("read")
        .expect("present")
        .manifest;
    assert_eq!(m1, m2);
}

#[test]
fn read_absent_commit_is_none() {
    let (_dir, store) = new_store();
    let absent = Hash::from_hex("0123456789abcdef0123456789abcdef01234567").expect("hash");
    assert!(store.read_commit(&absent).expect("read").is_none());
}

#[test]
fn invalid_trailers_are_rejected() {
    let (_dir, store) = new_store();
    let bad = [
        trailer("", "value"),
        trailer("Has Space", "value"),
        trailer("Has:Colon", "value"),
        trailer("-leading-dash", "value"),
        trailer("Token", ""),
        trailer("Token", "multi\nline"),
        trailer("Token", " padded "),
        trailer("Token", "control\u{7}char"),
    ];
    for pair in bad {
        let trailers = vec![pair.clone()];
        let mut new = NewCommit::new(b"m", "s", "msg");
        new.trailers = &trailers;
        match store.create_commit(&new) {
            Err(StoreError::InvalidTrailer { .. }) => {}
            other => panic!("expected InvalidTrailer for {pair:?}, got {other:?}"),
        }
    }
}

#[test]
fn empty_message_is_rejected() {
    let (_dir, store) = new_store();
    for message in ["", "   \n\n"] {
        let result = store.create_commit(&NewCommit::new(b"m", "s", message));
        assert!(result.is_err(), "empty message must be rejected");
    }
}

#[test]
fn invalid_signatures_are_rejected() {
    let (_dir, store) = new_store();
    let bad = [
        Signature {
            name: "".into(),
            email: "a@b".into(),
        },
        Signature {
            name: "evil <injector>".into(),
            email: "a@b".into(),
        },
        Signature {
            name: "ok".into(),
            email: "a@b>\n<forged".into(),
        },
    ];
    for sig in bad {
        let mut new = NewCommit::new(b"m", "s", "msg");
        new.author = sig.clone();
        match store.create_commit(&new) {
            Err(StoreError::InvalidSignature { .. }) => {}
            other => panic!("expected InvalidSignature for {sig:?}, got {other:?}"),
        }
    }
}

#[test]
fn oversized_manifest_is_rejected_on_create() {
    let (_dir, store) = new_capped_store(1024);
    let manifest = vec![0u8; 2048];
    let result = store.create_commit(&NewCommit::new(&manifest, "s", "msg"));
    assert!(matches!(result, Err(StoreError::ObjectTooLarge { .. })));
}

#[test]
fn read_commit_on_blob_is_wrong_kind_error() {
    let (_dir, store) = new_store();
    let blob = store
        .put(b"garbage bytes where a commit is expected")
        .expect("put");
    match store.read_commit(&blob) {
        Err(StoreError::WrongObjectKind { expected, .. }) => assert_eq!(expected, "commit"),
        other => panic!("expected WrongObjectKind, got {other:?}"),
    }
}

#[test]
fn syntactically_broken_commit_object_is_error_not_panic() {
    // `hash-object --literally` lets us store an object of type commit
    // whose payload is garbage — exactly what a hostile repository can
    // contain. Decoding must fail cleanly.
    let (dir, store) = new_store();
    let hex = git_stdin(
        &repo_path(&dir),
        &[
            "hash-object",
            "-w",
            "-t",
            "commit",
            "--literally",
            "--stdin",
        ],
        b"this is not a commit at all\n",
    );
    let hash = Hash::from_hex(hex.trim()).expect("hash");
    match store.read_commit(&hash) {
        Err(StoreError::Corrupt { context, .. }) => assert_eq!(context, "commit object"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn commit_whose_tree_lacks_manifest_is_corrupt() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    // An empty tree, then a real commit over it — valid git, invalid acetone.
    let tree = git_stdin(&repo, &["mktree"], b"");
    let commit = git_stdin(
        &repo,
        &["commit-tree", tree.trim(), "-m", "no manifest here"],
        b"",
    );
    let hash = Hash::from_hex(commit.trim()).expect("hash");
    match store.read_commit(&hash) {
        Err(StoreError::Corrupt { context, reason }) => {
            assert_eq!(context, "commit tree");
            assert!(reason.contains("manifest"), "reason: {reason}");
        }
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn commit_with_missing_manifest_blob_is_corrupt() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    // A tree entry pointing at an object that does not exist.
    let bogus = "0123456789abcdef0123456789abcdef01234567";
    let tree = git_stdin(
        &repo,
        &["mktree", "--missing"],
        format!("100644 blob {bogus}\tmanifest\n").as_bytes(),
    );
    let commit = git_stdin(
        &repo,
        &["commit-tree", tree.trim(), "-m", "dangling manifest"],
        b"",
    );
    let hash = Hash::from_hex(commit.trim()).expect("hash");
    match store.read_commit(&hash) {
        Err(StoreError::Corrupt { context, .. }) => assert_eq!(context, "commit manifest"),
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn readers_ignore_unknown_tree_entries() {
    // Forward compatibility: a future version may add entries to the
    // commit tree; today's reader must not choke on them.
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let manifest = b"manifest bytes".as_slice();
    let manifest_hex = git_stdin(&repo, &["hash-object", "-w", "--stdin"], manifest);
    let extra_hex = git_stdin(&repo, &["hash-object", "-w", "--stdin"], b"from the future");
    let tree = git_stdin(
        &repo,
        &["mktree"],
        format!(
            "100644 blob {e}\tfuture-metadata\n100644 blob {m}\tmanifest\n100644 blob {e}\tzz-trailing-entry\n",
            m = manifest_hex.trim(),
            e = extra_hex.trim()
        )
        .as_bytes(),
    );
    let commit = git_stdin(
        &repo,
        &["commit-tree", tree.trim(), "-m", "commit from the future"],
        b"",
    );
    let hash = Hash::from_hex(commit.trim()).expect("hash");
    let read = store.read_commit(&hash).expect("read").expect("present");
    assert_eq!(read.manifest.as_ref(), manifest);
}

#[test]
fn oversized_manifest_in_hostile_commit_is_rejected_on_read() {
    // A commit created under a permissive cap, read under a strict one:
    // the manifest blob's size must be checked before materialisation.
    let (dir, writer) = new_store();
    let manifest = vec![0u8; 512 * 1024];
    let id = writer
        .create_commit(&NewCommit::new(&manifest, "s", "big manifest"))
        .expect("create");
    drop(writer);

    let capped =
        GitStore::open_with(&repo_path(&dir), common::capped_options(4096)).expect("open capped");
    assert!(matches!(
        capped.read_commit(&id),
        Err(StoreError::ObjectTooLarge { .. })
    ));
}
