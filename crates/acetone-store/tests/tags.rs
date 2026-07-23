//! Annotated-tag object plumbing (acetone-ejj): `read_tag` decodes what real
//! git wrote, `rewrite_tag` writes what real git can read — preserving
//! name, tagger (identity and timestamp) and message while repointing the
//! target — and signed tags are refused rather than silently stripped.

mod common;

use acetone_store::{CommitStore, GitStore, Hash, NewCommit, RefStore, StoreError};
use common::{git, git_stdin, new_store, repo_path};

/// One minimal acetone commit to hang tags off.
fn commit(store: &GitStore, message: &str) -> Hash {
    store
        .create_commit(&NewCommit::new(b"manifest", "# summary\n", message))
        .expect("create_commit")
}

#[test]
fn read_tag_decodes_a_git_created_annotated_tag() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let c = commit(&store, "first");
    git(
        &repo,
        &["tag", "-a", "v1", "-m", "release one", &c.to_hex()],
    );

    let tag_id = store
        .read_ref("refs/tags/v1")
        .expect("read_ref")
        .expect("tag ref");
    assert_ne!(tag_id, c, "an annotated tag ref names the tag object");
    let tag = store
        .read_tag(&tag_id)
        .expect("read_tag")
        .expect("a tag object");
    assert_eq!(tag.target, c);
    assert_eq!(tag.name, "v1");
    assert_eq!(tag.message.trim_end(), "release one");
    let tagger = tag.tagger.expect("git records a tagger");
    assert_eq!(tagger.name, "test");
    assert_eq!(tagger.email, "test@example.invalid");
    assert!(!tag.signed);
}

#[test]
fn read_tag_is_none_for_non_tags_and_absent_objects() {
    let (_dir, store) = new_store();
    let c = commit(&store, "first");
    // A commit is not a tag — identity fall-through, not an error.
    assert!(store.read_tag(&c).expect("read_tag").is_none());
    // Absence is absence.
    let absent = Hash::from_bytes(&[0xAB; 20]).expect("hash");
    assert!(store.read_tag(&absent).expect("read_tag").is_none());
}

#[test]
fn rewrite_tag_preserves_metadata_and_repoints_the_target() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let c1 = commit(&store, "first");
    let c2 = commit(&store, "second");
    git(
        &repo,
        &["tag", "-a", "v1", "-m", "release one", &c1.to_hex()],
    );
    let old_id = store
        .read_ref("refs/tags/v1")
        .expect("read_ref")
        .expect("tag ref");
    let old = store
        .read_tag(&old_id)
        .expect("read_tag")
        .expect("tag object");

    let new_id = store.rewrite_tag(&old, &c2).expect("rewrite_tag");
    assert_ne!(new_id, old_id, "repointing changes the object");
    let new = store
        .read_tag(&new_id)
        .expect("read_tag")
        .expect("tag object");
    assert_eq!(new.target, c2);
    // Name, message and tagger — identity AND timestamp — are preserved.
    assert_eq!(new.name, old.name);
    assert_eq!(new.message, old.message);
    assert_eq!(new.tagger, old.tagger);
    assert!(!new.signed);

    // Nested: rewriting a tag onto another TAG object records the right
    // target kind, and real git accepts the whole chain.
    let nested_id = store.rewrite_tag(&old, &new_id).expect("nested rewrite");
    let nested = store
        .read_tag(&nested_id)
        .expect("read_tag")
        .expect("tag object");
    assert_eq!(nested.target, new_id);
    git(&repo, &["update-ref", "refs/tags/v2", &new_id.to_hex()]);
    git(
        &repo,
        &["update-ref", "refs/tags/v2-nested", &nested_id.to_hex()],
    );
    git(&repo, &["fsck", "--strict"]);
    // git peels the nested chain to the repointed commit.
    let peeled = git(&repo, &["rev-parse", "refs/tags/v2-nested^{commit}"]);
    assert_eq!(peeled.trim(), c2.to_hex());
}

#[test]
fn rewrite_tag_refuses_a_signed_tag() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let c1 = commit(&store, "first");
    let c2 = commit(&store, "second");
    let content = format!(
        "object {}\ntype commit\ntag sealed\n\
         tagger T <t@example.invalid> 1700000000 +0000\n\nmsg\n\
         -----BEGIN PGP SIGNATURE-----\n\nAAAA\n-----END PGP SIGNATURE-----\n",
        c1.to_hex()
    );
    let id = git_stdin(
        &repo,
        &["hash-object", "-t", "tag", "-w", "--stdin"],
        content.as_bytes(),
    );
    let tag = store
        .read_tag(&Hash::from_hex(id.trim()).expect("hash"))
        .expect("read_tag")
        .expect("tag object");
    assert!(tag.signed, "the signature block must be detected");

    match store.rewrite_tag(&tag, &c2) {
        Err(StoreError::SignedTag { name }) => assert_eq!(name, "sealed"),
        other => panic!("expected SignedTag, got {other:?}"),
    }
}

#[test]
fn rewrite_tag_requires_the_new_target_to_exist() {
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let c1 = commit(&store, "first");
    git(&repo, &["tag", "-a", "v1", "-m", "m", &c1.to_hex()]);
    let tag_id = store
        .read_ref("refs/tags/v1")
        .expect("read_ref")
        .expect("tag ref");
    let tag = store
        .read_tag(&tag_id)
        .expect("read_tag")
        .expect("tag object");
    let absent = Hash::from_bytes(&[0xCD; 20]).expect("hash");
    match store.rewrite_tag(&tag, &absent) {
        Err(StoreError::Corrupt { .. }) => {}
        other => panic!("expected Corrupt for an absent target, got {other:?}"),
    }
}
