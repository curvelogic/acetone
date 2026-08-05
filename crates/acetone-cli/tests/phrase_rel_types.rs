//! Arbitrary relationship-type names end-to-end (`acetone-yx1o.2`):
//! open-vocabulary predicates are phrases, not identifiers — spaces,
//! unicode, punctuation — and must survive every shipped surface:
//! declaration (imperative, autodeclare, schema apply), Cypher backtick
//! quoting, matching, schema render, and fsck.

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

fn seeded_repo(dir: &tempfile::TempDir) -> std::path::PathBuf {
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
    ok(&acetone(&repo, &["declare-label", "Entity", "--key", "id"]));
    for id in ["1", "2"] {
        ok(&acetone(&repo, &["put-node", "Entity", id]));
    }
    repo
}

/// The phrase vocabulary an open-vocabulary tenant actually coins.
const PHRASES: &[&str] = &[
    "was influenced by",
    "wrote to",
    "co-occurs-with",
    "s'oppose \u{00e0}",
    "\u{5f71}\u{54cd}",
];

#[test]
fn phrase_names_survive_declare_create_match_and_fsck() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    for phrase in PHRASES {
        ok(&acetone(&repo, &["declare-rel-type", phrase]));
        let create = format!(
            "MATCH (a:Entity {{id: 1}}), (b:Entity {{id: 2}}) CREATE (a)-[:`{phrase}`]->(b)"
        );
        ok(&acetone(&repo, &["query", &create]));
        let read = format!("MATCH (:Entity {{id: 1}})-[:`{phrase}`]->(b) RETURN b.id");
        let out = ok(&acetone(&repo, &["query", &read]));
        // "1 row" pins that the TYPE filter matched exactly this edge —
        // every phrase connects the same pair, so a filter-ignoring
        // implementation would return more (PR #248 review minor 4).
        assert!(
            out.contains('2') && out.contains("1 row"),
            "{phrase}: {out}"
        );
    }
    // Negative control: a declared-but-never-created phrase matches nothing.
    ok(&acetone(&repo, &["declare-rel-type", "never used"]));
    let out = ok(&acetone(
        &repo,
        &["query", "MATCH ()-[:`never used`]->(b) RETURN b.id"],
    ));
    assert!(out.contains("0 rows"), "negative control: {out}");
    let schema = ok(&acetone(&repo, &["schema"]));
    for phrase in PHRASES {
        assert!(
            schema.contains(phrase),
            "schema must render {phrase}: {schema}"
        );
    }
    assert!(ok(&acetone(&repo, &["fsck"])).contains("clean"));
}

#[test]
fn phrase_names_coin_under_autodeclare() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let create = "MATCH (a:Entity {id: 1}), (b:Entity {id: 2}) \
                  CREATE (a)-[:`argues against`]->(b)";
    let out = acetone(&repo, &["query", "--autodeclare", create]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        format!("{}{}", stdout(&out), stderr(&out)).contains("argues against"),
        "the coinage advisory must carry the phrase"
    );
    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(schema.contains("argues against"), "{schema}");
}

#[test]
fn phrase_names_round_trip_through_schema_apply() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = seeded_repo(&dir);
    // Every phrase shape — spaces, unicode, punctuation — not just the
    // ASCII one (PR #248 review minor 5).
    for phrase in PHRASES {
        ok(&acetone(&source, &["declare-rel-type", phrase]));
    }
    ok(&acetone(
        &source,
        &["declare-rel-type", "was influenced by"],
    ));
    let doc = ok(&acetone(&source, &["schema", "--json"]));
    assert!(doc.contains("was influenced by"), "{doc}");

    let target_dir = tempfile::tempdir().expect("tempdir2");
    let target = seeded_repo(&target_dir);
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, &doc).expect("write doc");
    ok(&acetone(
        &target,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));
    let re_export = ok(&acetone(&target, &["schema", "--json"]));
    assert_eq!(
        doc, re_export,
        "phrase names must round-trip byte-identically"
    );
}
