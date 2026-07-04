//! End-to-end CLI test: drives the built `acetone` binary as a real
//! process through the full Phase 1 surface (bead acetone-63m.6), the
//! same way a user or the sprint-demo script would.

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

#[test]
fn scripted_session_exercises_every_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");

    // init (its own positional PATH argument overrides --repo).
    let out = init(&repo);
    assert!(out.status.success(), "init failed: {}", stderr(&out));
    assert!(stdout(&out).contains("Initialized empty acetone repository"));

    // status: fresh, unborn branch, clean, empty.
    let out = acetone(&repo, &["status"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("On branch main"));
    assert!(text.contains("HEAD: (no commits yet)"));
    assert!(text.contains("workspace: clean"));
    assert!(text.contains("nodes: 0, edges: 0, schema entries: 0"));

    // commit: refused, nothing staged (also covers the empty-root-commit
    // case — see the `Commit` subcommand's help text).
    let out = acetone(&repo, &["commit", "-m", "empty"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nothing to commit"));

    // put-node x2.
    let out = acetone(
        &repo,
        &[
            "put-node",
            "Person",
            "1",
            "--prop",
            "name=Alice",
            "--prop",
            "age=30",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("put node \"Person\" [1]"));

    let out = acetone(&repo, &["put-node", "Person", "2", "--prop", "name=Bob"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // put-edge.
    let out = acetone(&repo, &["put-edge", "Person", "1", "KNOWS", "Person", "2"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("put edge \"Person\" [1] -\"KNOWS\"-> \"Person\" [2]"));

    // status: now dirty, populated.
    let out = acetone(&repo, &["status"]);
    let text = stdout(&out);
    assert!(text.contains("workspace: dirty"));
    assert!(text.contains("nodes: 2, edges: 1, schema entries: 0"));

    // checkout while dirty is refused, not silently discarded.
    let out = acetone(&repo, &["checkout", "main"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("uncommitted changes"));

    // commit.
    let out = acetone(
        &repo,
        &[
            "commit",
            "-m",
            "seed data",
            "--trailer",
            "Acetone-Source=test",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let commit_line = stdout(&out);
    assert!(commit_line.starts_with("committed "));
    let commit_hex = commit_line
        .trim()
        .strip_prefix("committed ")
        .unwrap()
        .to_owned();
    assert!(!commit_hex.is_empty());

    // commit again with nothing new staged: refused, not a pointless
    // repeat commit (the blocker this test locks in).
    let out = acetone(&repo, &["commit", "-m", "again"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nothing to commit"));

    // status: clean again, head commit set.
    let out = acetone(&repo, &["status"]);
    let text = stdout(&out);
    assert!(text.contains("workspace: clean"));
    assert!(text.contains(&format!("HEAD: {commit_hex}")));

    // log: one entry, our message and trailer.
    let out = acetone(&repo, &["log"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains(&format!("{commit_hex} seed data")));
    assert!(text.contains("Acetone-Source: test"));

    // branch (list): only main, marked current.
    let out = acetone(&repo, &["branch"]);
    assert_eq!(stdout(&out).trim(), "* main");

    // branch (create).
    let out = acetone(&repo, &["branch", "feature"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("created branch \"feature\""));

    let out = acetone(&repo, &["branch"]);
    let text = stdout(&out);
    assert!(text.contains("* main"));
    assert!(text.contains("  feature"));

    // checkout.
    let out = acetone(&repo, &["checkout", "feature"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("switched to branch \"feature\""));

    let out = acetone(&repo, &["branch"]);
    assert!(stdout(&out).contains("* feature"));

    // get-node: found, with properties in a stable order.
    let out = acetone(&repo, &["get-node", "Person", "1"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("node: \"Person\" [1]"));
    assert!(text.contains("\"age\": 30"));
    assert!(text.contains("\"name\": \"Alice\""));

    // get-node: not found is a clean result, not an error.
    let out = acetone(&repo, &["get-node", "Person", "99"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out).trim(), "not found");

    // list-nodes, unfiltered and filtered.
    let out = acetone(&repo, &["list-nodes"]);
    let text = stdout(&out);
    assert!(text.contains("\"Person\" [1]"));
    assert!(text.contains("\"Person\" [2]"));

    let out = acetone(&repo, &["put-node", "Place", "1", "--prop", "name=Paris"]);
    assert!(out.status.success());
    let out = acetone(&repo, &["list-nodes", "--label", "Person"]);
    let text = stdout(&out);
    assert!(text.contains("\"Person\" [1]"));
    assert!(!text.contains("Place"));
}

#[test]
fn friendly_errors_not_debug_dumps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("nope");

    let out = acetone(&repo, &["status"]);
    assert!(!out.status.success());
    let text = stderr(&out);
    assert!(text.starts_with("error: "));
    // A Debug dump of GraphError would show enum/variant syntax like
    // `Store(Backend { .. })`; the Display chain must not.
    assert!(!text.contains("Backend {"));
    assert!(!text.contains("GraphError::"));
}

#[test]
fn malformed_prop_is_rejected_with_a_clear_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let out = init(&repo);
    assert!(out.status.success(), "init failed: {}", stderr(&out));

    let out = acetone(&repo, &["put-node", "Person", "1", "--prop", "noequals"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("expected KEY=VALUE"));
}

#[test]
fn control_characters_in_labels_are_escaped_not_raw() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let out = init(&repo);
    assert!(out.status.success(), "init failed: {}", stderr(&out));

    // A label containing an ANSI escape and a bell must never reach the
    // terminal unescaped (repo data is attacker-writable).
    let evil_label = "Evil\u{1b}[31mRed\u{7}Bell";
    let out = acetone(&repo, &["put-node", evil_label, "1"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains('\u{7}'));
    assert!(text.contains("\\u{1b}"));

    let out = acetone(&repo, &["get-node", evil_label, "1"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(!text.contains('\u{1b}'));
    assert!(text.contains("\\u{1b}"));
}
