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
            "--require",
            "cores",
            "--unique",
            "serial",
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
    let doc = r#"{
        "labels": [{"name": "Note", "surrogate": true, "types": {"text": "string"}}],
        "relationship_types": [
            {"name": "CALLED", "discriminator": "at", "types": {"at": "int"}, "required": ["at"]}
        ]
    }"#;
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
        status.contains("clean"),
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

/// The require/unique backfill check applies to `apply` exactly as it does
/// to `declare-label` (PR #246 review blocker 1): a declaration existing
/// data violates is refused, and nothing lands.
#[test]
fn apply_refuses_what_declare_label_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    ok(&acetone(
        &repo,
        &["declare-label", "Host", "--key", "hostname"],
    ));
    for host in ["web1", "web2"] {
        ok(&acetone(
            &repo,
            &["put-node", "Host", host, "--prop", "serial=\"S1\""],
        ));
    }

    let doc = r#"{
        "labels": [
            {"name": "Host", "key": ["hostname"], "unique": ["serial"]},
            {"name": "Rack", "key": ["id"]}
        ]
    }"#;
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, doc).expect("write doc");

    // The imperative path refuses...
    let direct = acetone(
        &repo,
        &[
            "declare-label",
            "Host",
            "--key",
            "hostname",
            "--unique",
            "serial",
        ],
    );
    assert!(!direct.status.success());

    // ...and apply must refuse identically, landing nothing.
    let out = acetone(&repo, &["schema", "apply", doc_path.to_str().unwrap()]);
    assert!(!out.status.success(), "the unique breach must refuse");
    assert!(
        stderr(&out).contains("nothing was applied"),
        "{}",
        stderr(&out)
    );
    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(!schema.contains("Rack"), "nothing may land: {schema}");
    assert!(!schema.contains("serial"), "nothing may land: {schema}");
}

/// Within an entry the document is desired state: omissions DROP facets,
/// and the plan says exactly what (PR #246 review blocker 2) — the
/// stripping is deliberate, announced behaviour, not an accident.
#[test]
fn omitting_a_facet_drops_it_and_the_plan_names_the_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    ok(&acetone(
        &repo,
        &[
            "declare-label",
            "Host",
            "--key",
            "hostname",
            "--type",
            "cores:int",
            "--unique",
            "serial",
        ],
    ));

    let doc = r#"{"labels": [{"name": "Host", "key": ["hostname"]}]}"#;
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, doc).expect("write doc");

    // Dry run names the drops without applying.
    let plan = ok(&acetone(
        &repo,
        &["schema", "apply", "--dry-run", doc_path.to_str().unwrap()],
    ));
    assert!(
        plan.contains("change (drops") && plan.contains("types") && plan.contains("unique"),
        "the plan must name what it drops: {plan}"
    );
    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(
        schema.contains("serial"),
        "dry run must not strip: {schema}"
    );

    // The real apply strips, as the plan said it would.
    ok(&acetone(
        &repo,
        &["schema", "apply", doc_path.to_str().unwrap()],
    ));
    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(
        !schema.contains("serial") && !schema.contains("cores"),
        "desired-state replacement must drop omitted facets: {schema}"
    );
}

/// The export flags are refused before `apply` rather than silently
/// ignored (PR #246 review minor 5).
#[test]
fn export_flags_are_refused_with_apply() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, r#"{"labels": []}"#).expect("write doc");
    for flags in [vec!["--json"], vec!["--at", "HEAD"]] {
        let mut args = vec!["schema"];
        args.extend(flags.iter().copied());
        args.extend(["apply", doc_path.to_str().unwrap()]);
        let out = acetone(&repo, &args);
        assert!(!out.status.success(), "{args:?} must refuse");
    }
}

/// Phase 10 security review minor 3: duplicate keys within one object are
/// refused, not silently last-wins.
#[test]
fn duplicate_json_keys_within_an_entry_are_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let doc_path = dir.path().join("schema.json");
    std::fs::write(
        &doc_path,
        r#"{"labels":[{"name":"Host","key":["hostname"],"name":"Evil","key":["other"]}]}"#,
    )
    .expect("write");
    let out = acetone(&repo, &["schema", "apply", doc_path.to_str().unwrap()]);
    assert!(!out.status.success(), "duplicate keys must refuse");
    assert!(stderr(&out).contains("duplicate key"), "{}", stderr(&out));
    let schema = ok(&acetone(&repo, &["schema"]));
    assert!(!schema.contains("Evil"), "nothing may land: {schema}");
}

/// Phase 10 security review minor 4: mid-merge, `apply` refuses outright —
/// a conflicted schema entry is absent from the merged map, so the diff
/// would report it as `add` and silently bulk-resolve conflicts by-write.
#[test]
fn apply_refuses_while_a_merge_is_unresolved() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    ok(&acetone(&repo, &["declare-label", "Doc", "--key", "id"]));
    ok(&acetone(&repo, &["commit", "-m", "base"]));
    ok(&acetone(&repo, &["branch", "side"]));

    // Divergent declarations of the same rel type on the two branches.
    ok(&acetone(
        &repo,
        &["declare-rel-type", "R", "--type", "a:int"],
    ));
    ok(&acetone(&repo, &["commit", "-m", "main R"]));
    ok(&acetone(&repo, &["checkout", "side"]));
    ok(&acetone(
        &repo,
        &["declare-rel-type", "R", "--type", "a:string"],
    ));
    ok(&acetone(&repo, &["commit", "-m", "side R"]));
    ok(&acetone(&repo, &["checkout", "main"]));
    let merge = acetone(&repo, &["merge", "side", "-m", "merge side"]);
    assert!(!merge.status.success(), "must conflict: {}", stdout(&merge));

    let doc_path = dir.path().join("schema.json");
    std::fs::write(&doc_path, r#"{"relationship_types":[{"name":"R"}]}"#).expect("write");
    let out = acetone(&repo, &["schema", "apply", doc_path.to_str().unwrap()]);
    assert!(!out.status.success(), "apply must refuse mid-merge");
    assert!(
        stderr(&out).contains("merge"),
        "the refusal must name the merge: {}",
        stderr(&out)
    );
}
