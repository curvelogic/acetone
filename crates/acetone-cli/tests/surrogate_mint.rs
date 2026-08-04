//! Surrogate labels end-to-end through the shipped binary
//! (`acetone-yx1o.4`): declare via `schema apply`, create through Cypher
//! with the ULID minted, survive fsck.

use std::path::Path;
use std::process::{Command, Output};

fn acetone(repo: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(bin).args(&full).output().expect("run acetone")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("utf8")
}
fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("utf8")
}
fn ok(o: &Output) -> String {
    assert!(o.status.success(), "{}{}", stdout(o), stderr(o));
    stdout(o)
}

#[test]
fn surrogate_declare_create_and_fsck_through_the_binary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let bin = env!("CARGO_BIN_EXE_acetone");
    assert!(
        Command::new(bin)
            .args(["init", repo.to_str().unwrap()])
            .output()
            .expect("init")
            .status
            .success()
    );
    let doc_path = dir.path().join("schema.json");
    std::fs::write(
        &doc_path,
        r#"{"labels": [{"name": "Note", "surrogate": true, "types": {"text": "string"}}]}"#,
    )
    .expect("write doc");
    ok(&acetone(
        &repo,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));

    // The shape the PR #246 review showed failing, now working:
    let out = ok(&acetone(
        &repo,
        &["query", "CREATE (n:Note {text: 'hi'}) RETURN n._id"],
    ));
    let id = out
        .split_whitespace()
        .map(|tok| tok.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .find(|tok| tok.len() == 26 && tok.chars().all(|c| c.is_ascii_alphanumeric()))
        .expect("a ULID token in the output")
        .to_string();
    assert_eq!(id.len(), 26, "ULID expected in: {out}");

    let read = ok(&acetone(
        &repo,
        &[
            "query",
            &format!("MATCH (n:Note {{_id: '{id}'}}) RETURN n.text"),
        ],
    ));
    assert!(read.contains("hi"), "{read}");

    let fsck = ok(&acetone(&repo, &["fsck"]));
    assert!(fsck.contains("clean"), "{fsck}");
}
