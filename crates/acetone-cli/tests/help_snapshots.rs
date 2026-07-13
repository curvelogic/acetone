//! Snapshot corpus for the CLI's user-facing text: `--help` output and the
//! stderr of common failure paths. Reuses the crate's existing subprocess
//! harness (`env!("CARGO_BIN_EXE_acetone")` + `Command`) rather than adding a
//! CLI-driver dependency.
//!
//! These baselines deliberately capture today's output — the paragraph-long
//! command descriptions, the ungrouped command list, the `[String("…")]` key
//! leaks — so the 0.1.1 help and error-message beads land as reviewed
//! `cargo insta review` diffs rather than silent changes.

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

/// Run `<bin> <args…>` with no `--repo` prefix (for top-level `--help`).
fn raw(args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    Command::new(bin).args(args).output().expect("run acetone")
}

/// Every subcommand whose `--help` we pin. `help` and `shell` are excluded:
/// `help` is trivial and `shell` would need a controlled TTY.
const SUBCOMMANDS: &[&str] = &[
    "init",
    "status",
    "commit",
    "log",
    "branch",
    "checkout",
    "merge",
    "declare-label",
    "declare-rel-type",
    "declare-index",
    "reindex",
    "export",
    "put-node",
    "rekey",
    "resolve",
    "diff",
    "get-node",
    "put-edge",
    "list-nodes",
    "query",
    "fsck",
    "gc",
    "migrate",
    "import",
];

#[test]
fn top_level_help_snapshot() {
    let out = raw(&["--help"]);
    assert!(out.status.success(), "acetone --help should succeed");
    insta::assert_snapshot!("top_level_help", stdout(&out));
}

#[test]
fn subcommand_help_snapshot() {
    let mut combined = String::new();
    for cmd in SUBCOMMANDS {
        let out = raw(&[cmd, "--help"]);
        assert!(out.status.success(), "acetone {cmd} --help should succeed");
        combined.push_str("========== acetone ");
        combined.push_str(cmd);
        combined.push_str(" --help ==========\n");
        combined.push_str(&stdout(&out));
        combined.push('\n');
    }
    insta::assert_snapshot!("subcommand_help", combined);
}

#[test]
fn failure_output_snapshot() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path();
    assert!(init(repo).status.success(), "init");

    // A schema for the duplicate-key case: declare a keyed label and commit it.
    assert!(
        acetone(repo, &["declare-label", "Topic", "--key", "name"])
            .status
            .success(),
        "declare-label"
    );
    assert!(
        acetone(repo, &["commit", "-m", "schema"]).status.success(),
        "commit schema"
    );
    // Create one Topic so the second create collides on the key.
    assert!(
        acetone(repo, &["query", "CREATE (:Topic {name: 'x'});"])
            .status
            .success(),
        "seed node"
    );

    // Each case: a short label and the argv (after the implicit --repo) to run.
    let cases: &[(&str, &[&str])] = &[
        ("unknown-subcommand", &["st"]),
        (
            "missing-colon-label",
            &["query", "CREATE (Topic {name: 'First'});"],
        ),
        (
            "schema-not-declared",
            &["query", "CREATE (:Widget {sku: 'a'});"],
        ),
        ("parse-error", &["query", "MATCH (n) RETRUN n"]),
        (
            "rel-missing-colon",
            &[
                "query",
                "CREATE (:Topic {name: 'a'})-[LINK]->(:Topic {name: 'b'});",
            ],
        ),
        ("duplicate-key", &["query", "CREATE (:Topic {name: 'x'});"]),
        ("get-node-not-found", &["get-node", "Topic", "missing"]),
        // NoSuchNode via the graph layer's key renderer (rekey a missing node).
        (
            "rekey-missing-node",
            &["rekey", "Topic", "absent", "present", "-m", "r"],
        ),
    ];

    let mut combined = String::new();
    for (label, args) in cases {
        let out = acetone(repo, args);
        combined.push_str("### ");
        combined.push_str(label);
        combined.push('\n');
        combined.push_str("argv:   acetone ");
        combined.push_str(&args.join(" "));
        combined.push('\n');
        combined.push_str(&format!("status: {}\n", out.status.code().unwrap_or(-1)));
        let err = stderr(&out);
        let out_s = stdout(&out);
        if !err.is_empty() {
            combined.push_str("stderr: ");
            combined.push_str(err.trim_end());
            combined.push('\n');
        }
        if !out_s.is_empty() {
            combined.push_str("stdout: ");
            combined.push_str(out_s.trim_end());
            combined.push('\n');
        }
        combined.push('\n');
    }
    insta::assert_snapshot!("failure_output", combined);
}
