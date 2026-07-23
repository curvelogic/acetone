//! End-to-end checks for the error-message quality pass (acetone-cbl.3):
//! the curated warts collected during the 0.3.0 manual verification, each
//! pinned here against the real binary so a regression is a test failure,
//! not a re-discovered annoyance.

use std::path::Path;
use std::process::{Command, Output};

fn acetone(repo: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full_args = vec!["--repo", repo.to_str().unwrap()];
    full_args.extend_from_slice(args);
    Command::new(bin)
        .args(&full_args)
        .output()
        .expect("failed to run acetone binary")
}

fn init(repo: &Path) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    Command::new(bin)
        .args(["init", repo.to_str().unwrap()])
        .output()
        .expect("init")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is not UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is not UTF-8")
}

/// A repository with one declared, committed label (`Company`, key `name`).
fn repo_with_schema(dir: &Path) -> std::path::PathBuf {
    let repo = dir.join("repo");
    let out = init(&repo);
    assert!(out.status.success(), "init: {}", stderr(&out));
    let out = acetone(&repo, &["declare-label", "Company", "--key", "name"]);
    assert!(out.status.success(), "declare-label: {}", stderr(&out));
    repo
}

// --- item 1: undeclared-label guidance at bind time -------------------------

#[test]
fn bind_time_unknown_label_without_near_miss_carries_declare_guidance() {
    // Fresh repo: the write-time path already explains Invariant #3 and how
    // to declare. Once any label exists the same mistake is caught earlier,
    // at bind time — that path must carry equivalent guidance, not a terse
    // "unknown label".
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_schema(dir.path());

    let out = acetone(&repo, &["query", "CREATE (:Person {name: 'A'})"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(
        text.contains("unknown label \"Person\""),
        "names the label: {text}"
    );
    assert!(
        text.contains("acetone declare-label \"Person\" --key"),
        "tells the user how to declare it: {text}"
    );
}

#[test]
fn bind_time_unknown_label_near_miss_still_suggests() {
    // The did-you-mean path is guidance enough; it must not be buried under
    // the declare hint (declaring a typo would be the wrong fix).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_schema(dir.path());

    let out = acetone(&repo, &["query", "MATCH (c:Compny) RETURN c.name"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(
        text.contains("did you mean \"Company\"?"),
        "near miss keeps the suggestion: {text}"
    );
    assert!(
        !text.contains("declare-label"),
        "no declare hint on a near miss: {text}"
    );
}

// --- item 2: no "(no columns)" before a write summary ------------------------

#[test]
fn write_only_query_prints_the_summary_without_a_no_columns_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_schema(dir.path());

    let out = acetone(&repo, &["query", "CREATE (:Company {name: 'Acme'})"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(
        text, "1 node created\n",
        "write-only output is exactly the mutation summary"
    );
}

#[test]
fn write_only_query_with_no_effect_still_reports_no_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_schema(dir.path());

    let out = acetone(&repo, &["query", "MATCH (c:Company) DELETE c"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "(no changes)\n");
}

#[test]
fn read_query_with_columns_is_unchanged_by_the_suppression() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_schema(dir.path());
    let out = acetone(&repo, &["query", "CREATE (:Company {name: 'Acme'})"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = acetone(&repo, &["query", "MATCH (c:Company) RETURN c.name"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("c.name"), "table renders: {text}");
    assert!(text.contains("1 row"), "row count renders: {text}");
}

// --- item 3: map projection gets a named, honest error ----------------------

#[test]
fn map_projection_error_names_the_unsupported_construct() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_schema(dir.path());

    let out = acetone(&repo, &["query", "MATCH (s:Company) RETURN s{.name}"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(
        text.contains("map projection"),
        "names the construct: {text}"
    );
    assert!(
        !text.contains("no clause may follow RETURN"),
        "the misleading clause-structure message is gone: {text}"
    );
}

// --- item 4: import no-op message matches what was actually checked ---------

#[test]
fn import_noop_message_is_graph_level_not_source_level() {
    // The no-op check is graph dirtiness after applying the source — the
    // source itself may never have been imported before (e.g. it repeats
    // rows the graph already holds), so "source unchanged" was a lie.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_schema(dir.path());
    let out = acetone(&repo, &["commit", "-m", "schema"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let csv = dir.path().join("companies.csv");
    std::fs::write(&csv, "name\nAcme\n").expect("write csv");
    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            "--label",
            "Company",
            csv.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "first import: {}", stderr(&out));

    let out = acetone(
        &repo,
        &[
            "import",
            "--format",
            "csv",
            "--label",
            "Company",
            csv.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "re-import: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("import produced no graph changes"),
        "message describes the graph-level check: {text}"
    );
    assert!(
        !text.contains("source unchanged"),
        "the source-level claim is gone: {text}"
    );
}

// --- items 5+6: nested errors render their cause once ------------------------

/// Count non-overlapping occurrences of `needle` in `haystack`.
fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

#[test]
fn init_refusal_on_a_non_empty_directory_prints_the_cause_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("stuff.txt"), "x").expect("write");

    let bin = env!("CARGO_BIN_EXE_acetone");
    let out = Command::new(bin)
        .args(["init", dir.path().to_str().unwrap()])
        .output()
        .expect("run init");
    assert!(!out.status.success());
    let text = stderr(&out);
    assert_eq!(
        occurrences(&text, "Refusing to initialize"),
        1,
        "the git backend's refusal must appear exactly once: {text}"
    );
}

#[test]
fn stale_ref_lock_error_prints_the_cause_once() {
    // A leftover acetone-refs.lock (the store-layer ref lock) makes ref
    // updates fail after the retry window; the underlying gitoxide message
    // must not be rendered twice (once inside StoreError::Backend's Display,
    // once again by the CLI's error-chain printer).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = repo_with_schema(dir.path());
    let out = acetone(&repo, &["commit", "-m", "schema"]);
    assert!(out.status.success(), "{}", stderr(&out));

    std::fs::write(repo.join("acetone-refs.lock"), "").expect("plant lock");
    let out = acetone(&repo, &["branch", "audit-fixes"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(
        text.contains("could not be obtained"),
        "the lock failure is reported: {text}"
    );
    assert_eq!(
        occurrences(&text, "could not be obtained"),
        1,
        "the lock failure must appear exactly once: {text}"
    );
}
