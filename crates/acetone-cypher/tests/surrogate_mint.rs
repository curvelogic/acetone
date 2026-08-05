//! Surrogate `_id` minting (spec §2, `acetone-yx1o.4`): CREATE on a
//! `KEY SURROGATE` label mints a ULID key at creation, visible to the
//! creating query's own rows.

use std::collections::BTreeMap;

use acetone_cypher::exec::value::Value as RtValue;
use acetone_cypher::session::{Outcome, Session};
use acetone_graph::repo::{InitOptions, Repository};
use acetone_model::schema::{LabelDef, SchemaEntry};

fn repo_with_note() -> (tempfile::TempDir, Repository) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo =
        Repository::init(&dir.path().join("graph.git"), InitOptions::default()).expect("init");
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Note".into(),
        def: LabelDef::surrogate(BTreeMap::new(), [], []).expect("surrogate def"),
    })
    .expect("schema");
    txn.save().expect("save");
    (dir, repo)
}

fn returned_string(outcome: Outcome) -> String {
    let result = match outcome {
        Outcome::Write(r) => r,
        Outcome::Read(r) => r,
    };
    assert_eq!(result.rows.len(), 1, "one row expected");
    match &result.rows[0][0] {
        RtValue::String(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn create_mints_a_ulid_visible_to_the_same_query() {
    let (_d, repo) = repo_with_note();
    let id = returned_string(
        Session::new(&repo)
            .run("CREATE (n:Note {text: 'hello'}) RETURN n._id")
            .expect("create"),
    );
    assert_eq!(id.len(), 26, "ULID is 26 chars: {id}");
    assert!(
        id.bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b)),
        "Crockford base32: {id}"
    );
    // And it is the persisted identity: match it back by key.
    let read = returned_string(
        Session::new(&repo)
            .run(&format!("MATCH (n:Note {{_id: '{id}'}}) RETURN n.text"))
            .expect("read back"),
    );
    assert_eq!(read, "hello");
}

#[test]
fn two_creates_mint_distinct_ids() {
    let (_d, repo) = repo_with_note();
    let outcome = Session::new(&repo)
        .run("CREATE (a:Note {text: 'a'}), (b:Note {text: 'b'}) RETURN a._id, b._id")
        .expect("create");
    let result = match outcome {
        Outcome::Write(r) => r,
        _ => panic!("write expected"),
    };
    let a = match &result.rows[0][0] {
        RtValue::String(s) => s.clone(),
        other => panic!("string expected: {other:?}"),
    };
    let b = match &result.rows[0][1] {
        RtValue::String(s) => s.clone(),
        other => panic!("string expected: {other:?}"),
    };
    assert_ne!(a, b, "distinct nodes need distinct ids");
}

#[test]
fn an_explicit_id_is_respected() {
    let (_d, repo) = repo_with_note();
    let id = returned_string(
        Session::new(&repo)
            .run("CREATE (n:Note {_id: 'chosen', text: 'x'}) RETURN n._id")
            .expect("create"),
    );
    assert_eq!(id, "chosen", "an explicit _id must not be overwritten");
}

#[test]
fn merge_matches_before_minting_again() {
    let (_d, repo) = repo_with_note();
    let session = Session::new(&repo);
    session
        .run("MERGE (n:Note {text: 'once'})")
        .expect("first merge creates");
    session
        .run("MERGE (n:Note {text: 'once'})")
        .expect("second merge matches");
    let outcome = session
        .run("MATCH (n:Note {text: 'once'}) RETURN count(n)")
        .expect("count");
    let result = match outcome {
        Outcome::Read(r) => r,
        _ => panic!("read expected"),
    };
    assert!(
        matches!(result.rows[0][0], RtValue::Int(1)),
        "no duplicate mint: {:?}",
        result.rows[0][0]
    );
}

#[test]
fn natural_key_labels_are_untouched() {
    let (_d, repo) = repo_with_note();
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("schema");
    txn.save().expect("save");
    // A natural-key CREATE missing its key still errors — no minting.
    let err = Session::new(&repo)
        .run("CREATE (h:Host {name: 'x'})")
        .unwrap_err();
    assert!(
        err.to_string().contains("missing key property"),
        "natural keys stay mandatory: {err}"
    );
}

#[test]
fn a_bare_merge_matches_existing_notes_without_minting() {
    let (_d, repo) = repo_with_note();
    let session = Session::new(&repo);
    session.run("CREATE (:Note {text: 'a'})").expect("seed");
    // MERGE (n:Note) matches ANY existing Note — no re-mint.
    session.run("MERGE (n:Note)").expect("bare merge");
    let outcome = session
        .run("MATCH (n:Note) RETURN count(n)")
        .expect("count");
    let result = match outcome {
        Outcome::Read(r) => r,
        _ => panic!("read expected"),
    };
    assert!(
        matches!(result.rows[0][0], RtValue::Int(1)),
        "bare MERGE must match, not mint: {:?}",
        result.rows[0][0]
    );
}

#[test]
fn merge_by_explicit_id_is_idempotent() {
    let (_d, repo) = repo_with_note();
    let session = Session::new(&repo);
    session
        .run("MERGE (n:Note {_id: 'X', text: 'x'})")
        .expect("first: creates with the explicit id");
    session
        .run("MERGE (n:Note {_id: 'X', text: 'x'})")
        .expect("second: matches by id");
    let outcome = session
        .run("MATCH (n:Note) RETURN count(n)")
        .expect("count");
    let result = match outcome {
        Outcome::Read(r) => r,
        _ => panic!("read expected"),
    };
    assert!(matches!(result.rows[0][0], RtValue::Int(1)));
}

/// The Invariant #3 guard: a surrogate label cannot combine with a
/// natural-keyed one — the mint can never displace a natural key.
#[test]
fn surrogate_plus_natural_label_is_refused() {
    let (_d, repo) = repo_with_note();
    let mut txn = repo.begin_write().expect("begin");
    txn.put_schema(&SchemaEntry::Label {
        name: "Host".into(),
        def: LabelDef::new(vec!["id".into()], BTreeMap::new(), [], []).expect("label"),
    })
    .expect("schema");
    txn.save().expect("save");
    let err = Session::new(&repo)
        .run("CREATE (n:Note:Host {text: 'x', id: 'h1'})")
        .unwrap_err();
    assert!(
        err.to_string().contains("more than one label"),
        "ambiguous identity must refuse: {err}"
    );
}
