//! Parallel edges end-to-end through the shipped binary
//! (`acetone-z093.5`): declare a discriminated type via `schema apply`,
//! create parallel edges through Cypher, read them back, survive fsck.

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
fn discriminated_type_parallel_edges_through_the_binary() {
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
    ok(&acetone(&repo, &["declare-label", "Doc", "--key", "id"]));
    for id in ["1", "2"] {
        ok(&acetone(&repo, &["put-node", "Doc", id]));
    }
    let doc_path = dir.path().join("schema.json");
    std::fs::write(
        &doc_path,
        r#"{"relationship_types": [
            {"name": "CITES", "discriminator": "run", "types": {"run": "string"}}
        ]}"#,
    )
    .expect("write doc");
    ok(&acetone(
        &repo,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));

    for run in ["r1", "r2"] {
        ok(&acetone(
            &repo,
            &[
                "query",
                &format!(
                    "MATCH (a:Doc {{id: 1}}), (b:Doc {{id: 2}}) \
                     CREATE (a)-[:CITES {{run: '{run}'}}]->(b)"
                ),
            ],
        ));
    }
    let count = ok(&acetone(
        &repo,
        &["query", "MATCH ()-[r:CITES]->() RETURN count(r)"],
    ));
    assert!(count.contains('2'), "two parallel edges: {count}");
    let sel = ok(&acetone(
        &repo,
        &[
            "query",
            "MATCH ()-[r:CITES]->() WHERE r.run = 'r1' RETURN r.run",
        ],
    ));
    assert!(sel.contains("r1") && sel.contains("1 row"), "{sel}");
    assert!(ok(&acetone(&repo, &["fsck"])).contains("clean"));
}
