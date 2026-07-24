//! Annotated-tag object plumbing (acetone-ejj): `read_tag` decodes what real
//! git wrote, `rewrite_tag` writes what real git can read — preserving
//! name, tagger (identity and timestamp) and message while repointing the
//! target — and signed tags are refused rather than silently stripped.

mod common;

use acetone_store::{CommitStore, GitStore, Hash, NewCommit, RefStore, Signature, StoreError};
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

/// Build a tag object whose message ends with `block`, via real git, and
/// read it back.
fn tag_with_block(
    store: &GitStore,
    repo: &std::path::Path,
    target: &Hash,
    name: &str,
    block: &str,
) -> acetone_store::TagObject {
    let content = format!(
        "object {}\ntype commit\ntag {name}\n\
         tagger T <t@example.invalid> 1700000000 +0000\n\nmsg\n{block}",
        target.to_hex()
    );
    let id = git_stdin(
        repo,
        &["hash-object", "-t", "tag", "-w", "--stdin"],
        content.as_bytes(),
    );
    store
        .read_tag(&Hash::from_hex(id.trim()).expect("hash"))
        .expect("read_tag")
        .expect("tag object")
}

#[test]
fn ssh_and_x509_signed_tags_are_detected_and_refused() {
    // gix 0.62's TagRef parses only OpenPGP blocks into `pgp_signature`;
    // `gpg.format=ssh` and `gpg.format=x509` signatures stay inside the
    // message. They must still read as signed — and refuse a rewrite —
    // or migrate would fold a now-invalid signature into the rewritten
    // tag's message.
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let c1 = commit(&store, "first");
    let c2 = commit(&store, "second");

    let ssh = tag_with_block(
        &store,
        &repo,
        &c1,
        "ssh-sealed",
        "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----\n",
    );
    assert!(ssh.signed, "an SSH signature block must be detected");
    match store.rewrite_tag(&ssh, &c2) {
        Err(StoreError::SignedTag { name }) => assert_eq!(name, "ssh-sealed"),
        other => panic!("expected SignedTag for SSH, got {other:?}"),
    }

    let x509 = tag_with_block(
        &store,
        &repo,
        &c1,
        "smime-sealed",
        "-----BEGIN SIGNED MESSAGE-----\nMIIB\n-----END SIGNED MESSAGE-----\n",
    );
    assert!(
        x509.signed,
        "an X.509/S-MIME signature block must be detected"
    );
    match store.rewrite_tag(&x509, &c2) {
        Err(StoreError::SignedTag { name }) => assert_eq!(name, "smime-sealed"),
        other => panic!("expected SignedTag for X.509, got {other:?}"),
    }

    // Defence in depth: even a hand-built TagObject claiming `signed:
    // false` cannot smuggle a signature block through `rewrite_tag`.
    let mut smuggled = ssh.clone();
    smuggled.signed = false;
    match store.rewrite_tag(&smuggled, &c2) {
        Err(StoreError::SignedTag { name }) => assert_eq!(name, "ssh-sealed"),
        other => panic!("expected SignedTag for a smuggled block, got {other:?}"),
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

#[test]
fn create_tag_writes_an_annotated_tag_git_can_read() {
    // ADR-0059 (acetone-ujsk): `create_tag` is the creation quarter of the
    // tag module — a fresh annotated tag object real git verifies.
    let (dir, store) = new_store();
    let repo = repo_path(&dir);
    let c = commit(&store, "first");

    let tag_id = store
        .create_tag("v1", &c, "release one", &Signature::default())
        .expect("create_tag");
    assert_ne!(tag_id, c, "a fresh tag OBJECT, not the commit");

    // Round-trips through our own reader…
    let tag = store
        .read_tag(&tag_id)
        .expect("read_tag")
        .expect("a tag object");
    assert_eq!(tag.target, c);
    assert_eq!(tag.name, "v1");
    assert_eq!(tag.message.trim_end(), "release one");
    assert!(!tag.signed);
    let tagger = tag.tagger.expect("tagger recorded");
    assert_eq!(tagger.name, "acetone");
    assert_eq!(tagger.email, "acetone@acetone.invalid");

    // …peels to the commit, and real git decodes and fscks it.
    assert_eq!(store.peel_tag(&tag_id).expect("peel"), c);
    store
        .write_ref("refs/tags/v1", None, &tag_id)
        .expect("write ref");
    let shown = git(&repo, &["cat-file", "-p", &tag_id.to_hex()]);
    assert!(shown.contains("tag v1"), "git decodes the object: {shown}");
    assert!(shown.contains("release one"));
    git(&repo, &["fsck", "--strict"]);
}

#[test]
fn create_tag_refuses_an_absent_target_and_an_empty_message() {
    let (_dir, store) = new_store();
    let c = commit(&store, "first");

    let absent = Hash::from_hex("0123456789abcdef0123456789abcdef01234567").expect("hex");
    assert!(matches!(
        store.create_tag("v1", &absent, "msg", &Signature::default()),
        Err(StoreError::Corrupt { .. })
    ));
    assert!(matches!(
        store.create_tag("v1", &c, "  \n", &Signature::default()),
        Err(StoreError::Corrupt { .. })
    ));
}

#[test]
fn create_tag_refuses_a_message_carrying_a_signature_block() {
    // Acetone never *creates* signed tags (migrate could not rewrite them);
    // a message that would make the object read as signed is refused, not
    // written as a lie.
    let (_dir, store) = new_store();
    let c = commit(&store, "first");
    let sneaky = "msg\n-----BEGIN PGP SIGNATURE-----\n\nAAAA\n-----END PGP SIGNATURE-----\n";
    assert!(matches!(
        store.create_tag("v1", &c, sneaky, &Signature::default()),
        Err(StoreError::SignedTagCreation { name }) if name == "v1"
    ));
}
