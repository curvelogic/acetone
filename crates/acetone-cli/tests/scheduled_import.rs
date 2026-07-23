//! Phase 5 exit (acetone-6g5.5): the scheduled-import simulation. Successive
//! snapshots of a mutating source are imported as commits; an unchanged
//! snapshot is a detected no-op; the `diff` between two runs is the change
//! report. Ties together import provenance, no-op detection and diff.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn acetone(repo: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(bin).args(&full).output().expect("run acetone")
}

fn init(repo: &Path) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    Command::new(bin)
        .args(["init", repo.to_str().unwrap()])
        .output()
        .expect("init")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("utf8")
}
fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("utf8")
}

/// Import a snapshot and return the committed hash, or `None` for a no-op.
fn import_snapshot(repo: &Path, dir: &Path, name: &str, body: &str) -> Option<String> {
    let path = dir.join(name);
    fs::write(&path, body).expect("write snapshot");
    let out = acetone(
        repo,
        &[
            "import",
            "--format",
            "ndjson",
            path.to_str().unwrap(),
            "--label",
            "Host",
        ],
    );
    assert!(out.status.success(), "import {name}: {}", stderr(&out));
    let text = stdout(&out);
    if text.contains("import produced no graph changes") {
        return None;
    }
    // "imported N node(s) ...; commit <hex>"
    let hex = text
        .rsplit("commit ")
        .next()
        .expect("commit hex")
        .trim()
        .to_owned();
    assert_eq!(hex.len(), 40, "unexpected commit hex in: {text}");
    Some(hex)
}

#[test]
fn scheduled_import_simulation() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    // Schema: a keyed Host label with an index (so the whole Phase 5 surface —
    // import, indexes, diff — is exercised).
    assert!(
        acetone(&repo, &["declare-label", "Host", "--key", "name"])
            .status
            .success()
    );
    assert!(
        acetone(
            &repo,
            &[
                "declare-index",
                "host_os",
                "--label",
                "Host",
                "--property",
                "os"
            ]
        )
        .status
        .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "schema"]).status.success());

    // Snapshot 1: three hosts.
    let v1 = import_snapshot(
        &repo,
        dir.path(),
        "snap1.ndjson",
        "{\"name\":\"web1\",\"os\":\"linux\"}\n\
         {\"name\":\"db1\",\"os\":\"linux\"}\n\
         {\"name\":\"cache1\",\"os\":\"linux\"}\n",
    )
    .expect("snapshot 1 commits");

    // Snapshot 2: web1's os changes; a new host appears; the rest is unchanged.
    let v2 = import_snapshot(
        &repo,
        dir.path(),
        "snap2.ndjson",
        "{\"name\":\"web1\",\"os\":\"windows\"}\n\
         {\"name\":\"db1\",\"os\":\"linux\"}\n\
         {\"name\":\"cache1\",\"os\":\"linux\"}\n\
         {\"name\":\"new1\",\"os\":\"linux\"}\n",
    )
    .expect("snapshot 2 commits");

    // Snapshot 3: the same content as snapshot 2 → a detected no-op, no commit.
    // Detection is content-based (workspace-vs-HEAD), so it holds even if the
    // bytes differ but the graph would not change.
    let v3 = import_snapshot(
        &repo,
        dir.path(),
        "snap3.ndjson",
        "{\"name\":\"web1\",\"os\":\"windows\"}\n\
         {\"name\":\"db1\",\"os\":\"linux\"}\n\
         {\"name\":\"cache1\",\"os\":\"linux\"}\n\
         {\"name\":\"new1\",\"os\":\"linux\"}\n",
    );
    assert!(v3.is_none(), "an unchanged snapshot must be a no-op");

    // The change report between run 1 and run 2 is `diff`.
    let report = acetone(&repo, &["diff", &v1, &v2]);
    assert!(report.status.success(), "diff: {}", stderr(&report));
    let text = stdout(&report);
    assert!(
        text.contains("~ node \"Host\" [\"web1\"]"),
        "expected modified web1: {text}"
    );
    assert!(
        text.contains("+ node \"Host\" [\"new1\"]"),
        "expected added new1: {text}"
    );
    // db1 and cache1 were unchanged, so they are not in the report.
    assert!(!text.contains("\"db1\""), "db1 should be unchanged: {text}");
    assert!(
        !text.contains("\"cache1\""),
        "cache1 should be unchanged: {text}"
    );

    // The mutation is reflected in queries: web1 is now found under os=windows.
    // (This query is planned as an IndexSeek on host_os; correctness is what we
    // assert here — fsck below guards the index's integrity.)
    let q = acetone(
        &repo,
        &["query", "MATCH (h:Host {os: 'windows'}) RETURN h.name"],
    );
    assert!(
        stdout(&q).contains("web1"),
        "the os mutation is not queryable: {}",
        stdout(&q)
    );

    // Provenance trailers on the two real import commits.
    let log = acetone(&repo, &["log"]);
    assert_eq!(
        stdout(&log).matches("Acetone-Source-Hash:").count(),
        2,
        "expected two import commits with provenance, got: {}",
        stdout(&log)
    );

    // The repository is intact after the whole simulation.
    assert!(acetone(&repo, &["fsck"]).status.success());
}
