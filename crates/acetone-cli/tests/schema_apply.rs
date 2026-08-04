//! End-to-end `schema apply` (acetone-yx1o.1): declarative, transactional,
//! idempotent consumption of the `schema --json` document through the
//! shipped binary — define/import/export closed with acetone's own JSON as
//! the interchange format.

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

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "{}{}", stdout(o), stderr(o));
    stdout(o)
}

/// Build a schema with every feature the document carries. The surrogate
/// label goes in via `apply` itself — the CLI has no imperative surrogate
/// declaration, making `apply` the first shipped surface for it.
fn build_rich_schema(dir: &Path, repo: &Path) {
    ok(&acetone(
        repo,
        &[
            "declare-label",
            "Host",
            "--key",
            "hostname",
            "--type",
            "hostname:string",
            "--type",
            "cores:int",
        ],
    ));
    ok(&acetone(
        repo,
        &["declare-rel-type", "ON_HOST", "--type", "since:int"],
    ));
    ok(&acetone(
        repo,
        &[
            "declare-index",
            "host-cores",
            "--label",
            "Host",
            "--property",
            "cores",
        ],
    ));
    let doc = r#"{"labels": [{"name": "Note", "surrogate": true, "types": {"text": "string"}}]}"#;
    let doc_path = dir.join("surrogate.json");
    std::fs::write(&doc_path, doc).expect("write surrogate doc");
    let out = ok(&acetone(
        repo,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));
    assert!(out.contains("label \"Note\": add"), "{out}");
}

#[test]
fn export_apply_export_round_trips_identically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    assert!(init(&source).status.success());
    assert!(init(&target).status.success());
    build_rich_schema(dir.path(), &source);

    let doc = ok(&acetone(&source, &["schema", "--json"]));
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, &doc).expect("write doc");

    let applied = ok(&acetone(
        &target,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));
    assert!(applied.contains("applied"), "{applied}");

    let re_exported = ok(&acetone(&target, &["schema", "--json"]));
    assert_eq!(doc, re_exported, "round-trip must be byte-identical");
}

#[test]
fn re_apply_is_idempotent_and_leaves_the_workspace_clean() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    build_rich_schema(dir.path(), &repo);
    ok(&acetone(&repo, &["commit", "-m", "schema"]));

    let doc = ok(&acetone(&repo, &["schema", "--json"]));
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, &doc).expect("write doc");

    let out = ok(&acetone(
        &repo,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));
    assert!(out.contains("(nothing to apply)"), "{out}");
    let status = ok(&acetone(&repo, &["status"]));
    assert!(
        status.contains("clean") || !status.contains("dirty"),
        "idempotent apply must not dirty the workspace: {status}"
    );
}

#[test]
fn additions_and_changes_are_reported_and_absences_left_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    ok(&acetone(
        &repo,
        &["declare-label", "Host", "--key", "hostname"],
    ));
    ok(&acetone(&repo, &["declare-rel-type", "LEGACY"]));

    // Document: widens Host with a type, adds a new label, says nothing
    // about LEGACY.
    let doc = r#"{
        "labels": [
            {"name": "Host", "key": ["hostname"], "types": {"cores": "int"}},
            {"name": "Rack", "key": ["id"]}
        ]
    }"#;
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, doc).expect("write doc");
    let out = ok(&acetone(
        &repo,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));
    assert!(out.contains("label \"Host\": change"), "{out}");
    assert!(out.contains("label \"Rack\": add"), "{out}");
    assert!(out.contains("left as-is"), "{out}");

    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(
        schema.contains("LEGACY"),
        "apply must never remove: {schema}"
    );
    assert!(schema.contains("Rack"));
}

#[test]
fn a_refused_change_rejects_the_whole_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    ok(&acetone(
        &repo,
        &["declare-label", "Host", "--key", "hostname"],
    ));
    ok(&acetone(
        &repo,
        &["put-node", "Host", "web", "--prop", "cores=\"many\""],
    ));

    // cores:int fails the declare-time backfill check (existing string
    // value), and the valid Rack addition must NOT land either.
    let doc = r#"{
        "labels": [
            {"name": "Host", "key": ["hostname"], "types": {"cores": "int"}},
            {"name": "Rack", "key": ["id"]}
        ]
    }"#;
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, doc).expect("write doc");
    let out = acetone(&repo, &["schema", "apply", doc_path.to_str().unwrap()]);
    assert!(!out.status.success(), "the backfill breach must refuse");
    assert!(
        stderr(&out).contains("nothing was applied"),
        "{}",
        stderr(&out)
    );
    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(
        !schema.contains("Rack"),
        "transactional apply must not half-land: {schema}"
    );
}

#[test]
fn dry_run_prints_the_plan_and_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let doc = r#"{"labels": [{"name": "Host", "key": ["hostname"]}]}"#;
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, doc).expect("write doc");
    let out = ok(&acetone(
        &repo,
        &["schema", "apply", "--dry-run", doc_path.to_str().unwrap()],
    ));
    assert!(out.contains("label \"Host\": add"), "{out}");
    assert!(out.contains("dry run"), "{out}");
    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(!schema.contains("Host"), "dry run must not apply: {schema}");
}

#[test]
fn unknown_fields_and_types_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let doc_path = dir.path().join("schema.json");

    std::fs::write(
        &doc_path,
        r#"{"labels": [{"name": "A", "key": ["k"], "keys": ["typo"]}]}"#,
    )
    .expect("write");
    let out = acetone(&repo, &["schema", "apply", doc_path.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("unknown field"), "{}", stderr(&out));

    std::fs::write(
        &doc_path,
        r#"{"labels": [{"name": "A", "key": ["k"], "types": {"p": "integer"}}]}"#,
    )
    .expect("write");
    let out = acetone(&repo, &["schema", "apply", doc_path.to_str().unwrap()]);
    assert!(!out.status.success(), "unknown property type must refuse");
}

#[test]
fn stdin_apply_works() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut child = Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "schema", "apply", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(br#"{"labels": [{"name": "Doc", "key": ["id"]}]}"#)
        .expect("write");
    let out = child.wait_with_output().expect("run");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(schema.contains("Doc"));
}
