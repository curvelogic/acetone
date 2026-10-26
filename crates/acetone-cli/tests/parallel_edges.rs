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

/// acetone-7qw.26 (ADR-0082): the merge-time mirror of the declare-time
/// discriminator stability check, through the shipped binary. A branch
/// with no CITES edges declares a discriminator (legal there); main writes
/// a CITES edge under the undiscriminated declaration; merging pairs them.
/// The edge surfaces as a `rel-stale-discriminator` conflict row, blocks
/// completion, and deleting it lets the merge commit.
#[test]
fn a_merged_discriminator_over_the_other_branchs_edges_is_a_conflict() {
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
    ok(&acetone(&repo, &["declare-rel-type", "CITES"]));
    for id in ["1", "2"] {
        ok(&acetone(&repo, &["put-node", "Doc", id]));
    }
    ok(&acetone(&repo, &["commit", "-m", "base"]));
    ok(&acetone(&repo, &["branch", "disc"]));
    // main: an edge under the undiscriminated declaration (a Null key).
    ok(&acetone(
        &repo,
        &[
            "query",
            "MATCH (a:Doc {id: 1}), (b:Doc {id: 2}) CREATE (a)-[:CITES {run: 'r1'}]->(b)",
        ],
    ));
    ok(&acetone(&repo, &["commit", "-m", "edge"]));
    // disc: no CITES edges here, so declaring the discriminator is legal.
    ok(&acetone(&repo, &["checkout", "disc"]));
    let doc_path = dir.path().join("schema.json");
    std::fs::write(
        &doc_path,
        r#"{"relationship_types": [{"name": "CITES", "discriminator": "run"}]}"#,
    )
    .expect("write doc");
    ok(&acetone(
        &repo,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));
    ok(&acetone(&repo, &["commit", "-m", "declare disc"]));
    ok(&acetone(&repo, &["checkout", "main"]));
    let merge = acetone(&repo, &["merge", "disc", "-m", "merge"]);
    assert!(
        !merge.status.success(),
        "the merge must not land cleanly: {}{}",
        stdout(&merge),
        stderr(&merge)
    );

    let rows = ok(&acetone(
        &repo,
        &[
            "query",
            "CALL acetone.conflicts() YIELD kind, label, property, base, ours \
             WHERE kind = 'rel-stale-discriminator' RETURN label, property, base, ours",
            "--format",
            "csv",
        ],
    ));
    assert!(
        rows.contains("CITES") && rows.contains("run"),
        "one row naming the type and the declared discriminator: {rows}"
    );
    assert_eq!(rows.lines().count(), 2, "header + one row: {rows}");

    let refused = acetone(&repo, &["commit", "-m", "finish"]);
    assert!(!refused.status.success(), "completion must refuse");
    assert!(
        stderr(&refused).contains("no discriminator"),
        "the refusal names the stale edge: {}",
        stderr(&refused)
    );

    // Repair: delete the stale edge, then the merge completes.
    ok(&acetone(
        &repo,
        &["query", "MATCH ()-[r:CITES]->() DELETE r"],
    ));
    ok(&acetone(&repo, &["commit", "-m", "finish"]));
    assert!(ok(&acetone(&repo, &["fsck"])).contains("clean"));
}
