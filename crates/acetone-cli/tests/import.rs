//! End-to-end `acetone import` tests: drive the real binary through CSV,
//! JSON/NDJSON, edge and branch-isolated imports, asserting provenance
//! trailers and no-op detection (spec §7, ADR-0021).

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

/// Declare a label (staged) and commit it, so the import has a schema and a
/// non-unborn branch.
fn declare_and_commit_label(repo: &Path, label: &str, key: &str) {
    let out = acetone(repo, &["declare-label", label, "--key", key]);
    assert!(out.status.success(), "declare-label: {}", stderr(&out));
    let out = acetone(repo, &["commit", "-m", "schema"]);
    assert!(out.status.success(), "commit schema: {}", stderr(&out));
}

#[test]
fn csv_import_records_trailers_and_detects_noop() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    declare_and_commit_label(&repo, "Host", "name");

    let csv = dir.path().join("hosts.csv");
    fs::write(&csv, "name,cores\nweb1,8\ndb1,16\n").expect("write csv");

    // First import: two nodes, provenance trailers on the commit.
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            csv.to_str().unwrap(),
            "--label",
            "Host",
        ],
    );
    assert!(out.status.success(), "import: {}", stderr(&out));
    assert!(
        stdout(&out).contains("imported 2 node(s) and 0 edge(s)"),
        "got: {}",
        stdout(&out)
    );

    let log = acetone(&repo, &["log"]);
    let text = stdout(&log);
    assert!(text.contains("Acetone-Source:"), "trailers: {text}");
    assert!(text.contains("Acetone-Extractor: csv"), "trailers: {text}");
    assert!(text.contains("Acetone-Source-Hash:"), "trailers: {text}");

    // Second import of the identical source: a detected no-op, no new commit.
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            csv.to_str().unwrap(),
            "--label",
            "Host",
        ],
    );
    assert!(out.status.success(), "reimport: {}", stderr(&out));
    assert!(
        stdout(&out).contains("import produced no graph changes"),
        "got: {}",
        stdout(&out)
    );

    // fsck stays clean.
    let out = acetone(&repo, &["fsck"]);
    assert!(out.status.success(), "fsck: {}", stderr(&out));
}

#[test]
fn ndjson_import_commits_new_nodes() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    declare_and_commit_label(&repo, "Host", "name");

    let nd = dir.path().join("hosts.ndjson");
    fs::write(&nd, "{\"name\":\"web1\"}\n{\"name\":\"db1\"}\n").expect("write");

    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "ndjson",
            nd.to_str().unwrap(),
            "--label",
            "Host",
        ],
    );
    assert!(out.status.success(), "ndjson import: {}", stderr(&out));
    assert!(
        stdout(&out).contains("imported 2 node(s)"),
        "{}",
        stdout(&out)
    );

    let list = acetone(&repo, &["list-nodes", "--label", "Host"]);
    let text = stdout(&list);
    assert!(text.contains("web1"), "{text}");
    assert!(text.contains("db1"), "{text}");
}

#[test]
fn edge_import_creates_relationships() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    declare_and_commit_label(&repo, "Host", "name");
    // Import the endpoint nodes first.
    let nodes = dir.path().join("hosts.csv");
    fs::write(&nodes, "name\nweb1\ndb1\n").expect("write");
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            nodes.to_str().unwrap(),
            "--label",
            "Host",
        ],
    );
    assert!(out.status.success(), "node import: {}", stderr(&out));

    // Declare the relationship type and commit it.
    let out = acetone(&repo, &["declare-rel-type", "PEERS_WITH"]);
    assert!(out.status.success(), "declare-rel: {}", stderr(&out));
    let out = acetone(&repo, &["commit", "-m", "rel schema"]);
    assert!(out.status.success(), "commit rel: {}", stderr(&out));

    // Import edges.
    let edges = dir.path().join("edges.csv");
    fs::write(&edges, "src,dst\nweb1,db1\n").expect("write");
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            edges.to_str().unwrap(),
            "--edge",
            "PEERS_WITH",
            "--from",
            "Host=src",
            "--to",
            "Host=dst",
        ],
    );
    assert!(out.status.success(), "edge import: {}", stderr(&out));
    assert!(
        stdout(&out).contains("imported 0 node(s) and 1 edge(s)"),
        "{}",
        stdout(&out)
    );

    // The edge is queryable.
    let q = acetone(
        &repo,
        &[
            "query",
            "MATCH (a:Host)-[:PEERS_WITH]->(b:Host) RETURN a.name, b.name",
        ],
    );
    assert!(q.status.success(), "query: {}", stderr(&q));
    let text = stdout(&q);
    assert!(text.contains("web1"), "{text}");
    assert!(text.contains("db1"), "{text}");
}

#[test]
fn branch_import_leaves_the_current_branch_unchanged() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    declare_and_commit_label(&repo, "Host", "name");

    let csv = dir.path().join("hosts.csv");
    fs::write(&csv, "name\nweb1\n").expect("write");
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            csv.to_str().unwrap(),
            "--label",
            "Host",
            "--branch",
            "ingest",
        ],
    );
    assert!(out.status.success(), "branch import: {}", stderr(&out));
    assert!(stdout(&out).contains("onto ingest"), "{}", stdout(&out));

    // Still on main; main has no imported node.
    let status = acetone(&repo, &["status"]);
    assert!(
        stdout(&status).contains("On branch main"),
        "{}",
        stdout(&status)
    );
    let list = acetone(&repo, &["list-nodes", "--label", "Host"]);
    assert!(
        !stdout(&list).contains("web1"),
        "main got the node: {}",
        stdout(&list)
    );

    // The node is on the `ingest` branch.
    let list = acetone(
        &repo,
        &["query", "MATCH (h:Host) RETURN h.name", "--at", "ingest"],
    );
    assert!(stdout(&list).contains("web1"), "{}", stdout(&list));
}

#[test]
fn dirty_workspace_import_is_refused() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    // Declare a label but DO NOT commit: the workspace is now dirty.
    let out = acetone(&repo, &["declare-label", "Host", "--key", "name"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let csv = dir.path().join("hosts.csv");
    fs::write(&csv, "name\nweb1\n").expect("write");
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            csv.to_str().unwrap(),
            "--label",
            "Host",
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("uncommitted changes"),
        "stderr: {}",
        stderr(&out)
    );
}

/// acetone-9gw: an import violating a declared `--require` constraint must
/// fail atomically — non-zero exit, nothing committed, workspace clean —
/// through both the CSV and JSON extractors, exactly as the Cypher write
/// path rejects the same node.
#[test]
fn constraint_violating_import_fails_atomically_for_csv_and_json() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let out = acetone(
        &repo,
        &[
            "declare-label",
            "Service",
            "--key",
            "name",
            "--require",
            "tier",
            "--unique",
            "ip",
        ],
    );
    assert!(out.status.success(), "declare: {}", stderr(&out));
    let out = acetone(&repo, &["commit", "-m", "schema"]);
    assert!(out.status.success(), "commit: {}", stderr(&out));
    let log_before = stdout(&acetone(&repo, &["log"]));

    // CSV: the `tier` column is absent entirely.
    let csv = dir.path().join("services.csv");
    fs::write(&csv, "name,ip\napi,10.0.0.1\n").expect("write csv");
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            csv.to_str().unwrap(),
            "--label",
            "Service",
        ],
    );
    assert!(
        !out.status.success(),
        "csv import must fail: {}",
        stdout(&out)
    );
    let err = stderr(&out);
    assert!(err.contains("\"api\""), "names the node: {err}");
    assert!(err.contains("\"tier\""), "names the property: {err}");
    assert!(err.contains("required"), "{err}");

    // JSON: same violation through the JSON extractor.
    let json = dir.path().join("services.json");
    fs::write(&json, r#"[{"name": "api", "ip": "10.0.0.1"}]"#).expect("write json");
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "json",
            json.to_str().unwrap(),
            "--label",
            "Service",
        ],
    );
    assert!(
        !out.status.success(),
        "json import must fail: {}",
        stdout(&out)
    );
    assert!(stderr(&out).contains("\"tier\""), "{}", stderr(&out));

    // Atomic: no commit landed, the workspace is clean, the node is absent.
    assert_eq!(stdout(&acetone(&repo, &["log"])), log_before);
    let status = acetone(&repo, &["status"]);
    assert!(
        stdout(&status).contains("clean"),
        "workspace: {}",
        stdout(&status)
    );
    let list = acetone(&repo, &["list-nodes", "--label", "Service"]);
    assert!(!stdout(&list).contains("api"), "{}", stdout(&list));

    // And fsck stays clean of errors.
    assert!(acetone(&repo, &["fsck"]).status.success());

    // A corrected source imports.
    fs::write(&csv, "name,tier,ip\napi,gold,10.0.0.1\n").expect("write csv");
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            csv.to_str().unwrap(),
            "--label",
            "Service",
        ],
    );
    assert!(out.status.success(), "fixed import: {}", stderr(&out));
}
