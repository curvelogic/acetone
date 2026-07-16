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

    // get-node: a miss exits non-zero (so scripts can detect absence), with
    // the message on stderr and stdout empty.
    let out = acetone(&repo, &["get-node", "Person", "99"]);
    assert!(!out.status.success());
    assert!(stdout(&out).trim().is_empty());
    assert!(stderr(&out).contains("not found"));

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

#[test]
fn fsck_reports_clean_and_detects_damage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let out = init(&repo);
    assert!(out.status.success(), "init failed: {}", stderr(&out));

    // Healthy repo, with some committed content: clean, exit 0.
    let out = acetone(&repo, &["put-node", "Host", "web1", "--prop", "os=linux"]);
    assert!(out.status.success());
    let out = acetone(&repo, &["commit", "-m", "seed"]);
    assert!(out.status.success());
    let out = acetone(&repo, &["fsck"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("fsck: clean"));

    // Surgically destroy the repository's objects, sparing only what `open`
    // needs to read the workspace manifest — the workspace tree, the reserved
    // `.acetone/` subtree, and the manifest blob it contains (huo, ADR-0023) —
    // so `open` still succeeds and fsck itself runs: fsck must then report
    // error findings for the missing chunks, exit non-zero, no Debug dump. (A
    // random victim won't do — a fresh repo also holds unreachable superseded
    // manifests whose loss fsck rightly ignores.)
    let workspace_tree = git_rev_parse(&repo, "refs/worktree/acetone/workspace");
    let acetone_subtree = git_rev_parse(&repo, "refs/worktree/acetone/workspace:.acetone");
    let manifest_blob = git_rev_parse(&repo, "refs/worktree/acetone/workspace:.acetone/manifest");
    let object_path = |oid: &str| repo.join("objects").join(&oid[..2]).join(&oid[2..]);
    let spared = [
        object_path(&workspace_tree),
        object_path(&acetone_subtree),
        object_path(&manifest_blob),
    ];
    for object in loose_objects(&repo.join("objects")) {
        if !spared.contains(&object) {
            std::fs::remove_file(&object).expect("remove object");
        }
    }
    let out = acetone(&repo, &["fsck"]);
    assert!(!out.status.success(), "fsck must fail on a damaged repo");
    let text = stdout(&out);
    assert!(
        text.contains("[error]"),
        "must print an error finding: {text}"
    );
    assert!(stderr(&out).contains("integrity"), "{}", stderr(&out));
}

fn git_rev_parse(repo: &Path, refname: &str) -> String {
    let out = Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "rev-parse", refname])
        .output()
        .expect("git rev-parse");
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .expect("hex")
        .trim()
        .to_owned()
}

/// Every loose-object file under `objects/` (two-hex-char fan-out
/// directories only).
fn loose_objects(objects: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(objects) else {
        return found;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let is_fanout = dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.len() == 2 && n.chars().all(|c| c.is_ascii_hexdigit()));
        if dir.is_dir() && is_fanout {
            for file in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if file.path().is_file() {
                    found.push(file.path());
                }
            }
        }
    }
    found
}

#[test]
fn log_sanitises_hostile_commit_messages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let out = init(&repo);
    assert!(out.status.success(), "init failed: {}", stderr(&out));
    let out = acetone(&repo, &["put-node", "Host", "web1"]);
    assert!(out.status.success());
    let out = acetone(&repo, &["commit", "-m", "seed"]);
    assert!(out.status.success());

    // Forge a hostile commit the way a malicious clone would arrive: same
    // tree (still a valid acetone commit), but a message and trailer full
    // of terminal escape sequences, spliced in with raw git plumbing.
    let head = git_rev_parse(&repo, "refs/heads/main");
    let tree = git_rev_parse(&repo, &format!("{head}^{{tree}}"));
    let hostile_message =
        "evil\u{1b}[8m hidden\u{7}\r spoof\n\nEvil-Trailer: \u{1b}]0;pwned\u{7}value";
    let forged = Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "commit-tree",
            &tree,
            "-m",
            hostile_message,
        ])
        // commit-tree needs a committer identity; CI runners have no
        // global git config, so supply one explicitly.
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@acetone.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@acetone.invalid")
        .output()
        .expect("git commit-tree");
    assert!(
        forged.status.success(),
        "git commit-tree failed: {}",
        String::from_utf8_lossy(&forged.stderr)
    );
    let forged_id = String::from_utf8(forged.stdout)
        .expect("hex")
        .trim()
        .to_owned();
    let out = Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "update-ref",
            "refs/heads/main",
            &forged_id,
        ])
        .output()
        .expect("git update-ref");
    assert!(out.status.success());

    let out = acetone(&repo, &["log"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        !text.contains('\u{1b}'),
        "raw ESC reached the terminal: {text:?}"
    );
    assert!(
        !text.contains('\u{7}'),
        "raw BEL reached the terminal: {text:?}"
    );
    assert!(
        text.contains("\\u{1b}"),
        "escapes must be visible, not stripped"
    );
    assert!(text.contains("Evil-Trailer"), "trailer still listed");
}

#[test]
fn get_node_escapes_hostile_secondary_labels() {
    use acetone_core::graph::{InitOptions, Repository};
    use acetone_core::model::Value;
    use acetone_core::model::graph_keys::NodeKey;
    use acetone_core::model::records::NodeRecord;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    // Write a hostile secondary label through the library, the way a
    // malicious clone's data would arrive (the CLI cannot author one).
    let repository = Repository::init(&repo, InitOptions::default()).expect("init");
    let key = NodeKey::new("Host", vec![Value::String("web1".into())]).expect("valid");
    let record = NodeRecord::new(
        ["z\u{1b}]0;PWNED\u{7}\u{1b}[31mred".to_owned()],
        Default::default(),
    );
    let mut tx = repository.begin_write().expect("begin");
    tx.put_node(&key, &record).expect("put");
    tx.save().expect("save");
    drop(repository);

    let out = acetone(&repo, &["get-node", "Host", "web1"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        !text.contains('\u{1b}'),
        "raw ESC reached the terminal: {text:?}"
    );
    assert!(
        !text.contains('\u{7}'),
        "raw BEL reached the terminal: {text:?}"
    );
    assert!(text.contains("\\u{1b}"), "escaped form must be visible");
}

/// Regression for acetone-8ng: when the stdout consumer exits early
/// (`acetone log | grep -q`, `| head -1`), the CLI must terminate
/// cleanly instead of panicking with "failed printing to stdout:
/// Broken pipe". We reproduce the pipe close directly: spawn `log` with
/// piped stdout and drop the read end before the child writes.
#[test]
fn closed_stdout_pipe_is_a_clean_exit_not_a_panic() {
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let out = init(&repo);
    assert!(out.status.success(), "{}", stderr(&out));

    // A commit so `log` has something to print.
    let out = acetone(&repo, &["put-node", "Host", "web1", "--prop", "os=debian"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = acetone(&repo, &["commit", "-m", "seed commit for pipe test"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut child = std::process::Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "log"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn acetone log");

    // Close the read end immediately: every subsequent write in the
    // child gets EPIPE.
    drop(child.stdout.take());

    let status = child.wait().expect("wait");
    let mut errtext = String::new();
    use std::io::Read;
    child
        .stderr
        .take()
        .expect("stderr piped")
        .read_to_string(&mut errtext)
        .expect("read stderr");

    assert!(
        !errtext.contains("panicked") && !errtext.contains("Broken pipe"),
        "CLI panicked on closed stdout: {errtext}"
    );
    assert!(
        status.success(),
        "expected clean exit, got {status}: {errtext}"
    );
}

/// The one-shot `query --format table` command must never cap its output:
/// a scripted query piped to a file has to receive every row (the row cap is
/// interactive-shell-only).
#[test]
fn query_command_table_is_never_row_capped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    let out = acetone(
        &repo,
        &[
            "query",
            "UNWIND range(1, 1500) AS n RETURN n",
            "--format",
            "table",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("1500 rows"), "true total reported: {text}");
    assert!(
        !text.contains("more rows"),
        "one-shot table output must not be capped: {text}"
    );
    // A row beyond the shell cap of 1000 is present.
    assert!(
        text.contains("│ 1200 "),
        "row past shell cap is shown: {text}"
    );
}

/// acetone-7bn.5: in a schema-free repository an undeclared label in a `MATCH`
/// is not an error (openCypher read semantics), so a typo returns 0 rows with
/// no signal — an exploration trap. The query still returns 0 rows and exits 0,
/// but a non-error advisory naming the label lands on stderr.
#[test]
fn undeclared_label_match_in_a_schema_free_repo_advises_on_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    let out = acetone(&repo, &["query", "MATCH (n:Nope) RETURN n"]);
    // Result semantics unchanged: 0 rows, exit 0.
    assert!(out.status.success(), "must exit 0: {}", stderr(&out));
    assert!(stdout(&out).contains("0 rows"), "stdout: {}", stdout(&out));
    // The advisory is on stderr, names the label, and does not pollute stdout.
    let err = stderr(&out);
    assert!(err.contains("Nope"), "advisory must name the label: {err}");
    assert!(err.contains("not declared"), "stderr: {err}");
    assert!(
        !stdout(&out).contains("Nope"),
        "advisory must not pollute stdout: {}",
        stdout(&out)
    );

    // A label-free MATCH gets no advisory (nothing undeclared was referenced).
    let plain = acetone(&repo, &["query", "MATCH (n) RETURN n"]);
    assert!(plain.status.success());
    assert!(
        stderr(&plain).trim().is_empty(),
        "no advisory for a label-free match: {}",
        stderr(&plain)
    );
}

/// The `query` command (acetone-yzc.6): parse → bind → execute an
/// openCypher read query against the repository, in table/JSON/CSV.
#[test]
fn query_command_runs_cypher_over_the_graph() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    for (label, key, prop) in [
        ("Host", "web-01", "os=debian"),
        ("Host", "web-02", "os=ubuntu"),
        ("Software", "nginx", "vendor=f5"),
    ] {
        let out = acetone(&repo, &["put-node", label, key, "--prop", prop]);
        assert!(out.status.success(), "{}", stderr(&out));
    }
    let out = acetone(
        &repo,
        &["put-edge", "Host", "web-01", "RUNS", "Software", "nginx"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(acetone(&repo, &["commit", "-m", "seed"]).status.success());

    // Table output, ordered.
    let out = acetone(
        &repo,
        &["query", "MATCH (h:Host) RETURN h.os AS os ORDER BY os"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("debian"));
    assert!(text.contains("ubuntu"));
    assert!(text.find("debian").unwrap() < text.find("ubuntu").unwrap());
    assert!(text.contains("2 rows"));

    // Expansion + aggregate.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host)-[:RUNS]->(s:Software) RETURN count(*) AS n",
            "--format",
            "csv",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "n\n1");

    // JSON output shape.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (s:Software) RETURN s.vendor AS vendor",
            "--format",
            "json",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("\"vendor\": \"f5\""));
}

#[test]
fn query_at_ref_is_whole_query_time_travel() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    assert!(
        acetone(&repo, &["put-node", "Host", "a", "--prop", "x=1"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["commit", "-m", "one host"])
            .status
            .success()
    );
    let first = {
        let out = acetone(&repo, &["log"]);
        stdout(&out)
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string()
    };
    assert!(
        acetone(&repo, &["put-node", "Host", "b", "--prop", "x=2"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["commit", "-m", "two hosts"])
            .status
            .success()
    );

    let now = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN count(*) AS n",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&now).trim(), "n\n2");
    let past = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN count(*) AS n",
            "--format",
            "csv",
            "--at",
            &first,
        ],
    );
    assert_eq!(stdout(&past).trim(), "n\n1");
}

#[test]
fn query_errors_render_with_line_and_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    let out = acetone(&repo, &["query", "MATCH (n) RETURN"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.starts_with("error: "));
    assert!(err.contains("line 1, column"), "{err}");
}

/// Query output must neutralise repository-controlled terminal-escape
/// sequences (PR #25's bar; regression guard for PR #35). Labels and
/// string property values are attacker-writable via a hostile clone.
#[test]
fn query_output_sanitises_hostile_strings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    let evil_label = "Host\u{1b}[31mHACK";
    let out = acetone(
        &repo,
        &[
            "put-node",
            evil_label,
            "1",
            // ESC (C0), DEL (0x7f) and a C1 control (0x9b, CSI) — all must
            // be neutralised in every output format.
            "--prop",
            "note=ok\u{1b}[31mred\u{7f}\u{9b}m",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // Table: property value and label must be escaped, not raw.
    let out = acetone(&repo, &["query", "MATCH (n) RETURN n.note AS note, n"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(!text.contains('\u{1b}'), "raw ESC reached table output");
    assert!(!text.contains('\u{7f}'), "raw DEL reached table output");
    assert!(!text.contains('\u{9b}'), "raw C1 CSI reached table output");
    assert!(text.contains("\\u{1b}"), "escaped form must be visible");

    // CSV: same guarantee.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (n) RETURN n.note AS note",
            "--format",
            "csv",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(!text.contains('\u{1b}'), "raw ESC reached CSV output");

    // JSON: control characters must be \u-escaped.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (n) RETURN n.note AS note",
            "--format",
            "json",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(!text.contains('\u{1b}'), "raw ESC reached JSON output");
    assert!(text.contains("\\u001b"), "JSON must \\u-escape the ESC");
    // JSON must also escape DEL (0x7f) and C1 controls (0x80..=0x9f),
    // which C0-only escaping missed (Phase 2 security review MINOR-1:
    // align json_string with sanitise_line's coverage). These bytes are
    // the ones that regress if the fix is reverted.
    assert!(!text.contains('\u{7f}'), "raw DEL reached JSON output");
    assert!(!text.contains('\u{9b}'), "raw C1 CSI reached JSON output");
    assert!(text.contains("\\u007f"), "JSON must \\u-escape DEL");
    assert!(text.contains("\\u009b"), "JSON must \\u-escape the C1 CSI");
}

/// The shell `:log` command must sanitise commit subjects (Phase 2
/// security review MAJOR-3: a hostile clone's escape-bearing commit
/// subject must not reach the terminal raw through the REPL).
#[test]
fn shell_log_sanitises_hostile_commit_subjects() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["put-node", "Host", "a", "--prop", "x=1"])
            .status
            .success()
    );
    let out = acetone(&repo, &["commit", "-m", "seed\u{1b}[31mHACK\u{7}"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut child = Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "shell"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shell");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b":log\n:quit\n")
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains('\u{1b}'),
        "raw ESC reached shell :log output"
    );
    assert!(!text.contains('\u{7}'), "raw BEL reached shell :log output");
    assert!(
        text.contains("\\u{1b}"),
        "escaped form must be visible: {text}"
    );
}

/// The shell REPL runs queries and meta-commands from stdin.
#[test]
fn shell_runs_queries_and_meta_commands() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["put-node", "Host", "a", "--prop", "x=1"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "Host", "b", "--prop", "x=2"])
            .status
            .success()
    );

    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut child = Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "shell"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shell");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b":format csv\nMATCH (h:Host) RETURN count(*) AS n;\n:quit\n")
        .expect("write to shell");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("utf8");
    // The CSV result of the count query appears in the transcript.
    assert!(
        text.contains("\nn\n2") || text.contains("n\n2"),
        "shell transcript: {text}"
    );
}

/// Clause-group AT (acetone-yzc.7): `MATCH ... AT <ref>` inside a query
/// reads that clause's patterns from the graph at the given commit,
/// while the rest of the query sees the base version.
#[test]
fn query_clause_group_at_reads_a_past_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    assert!(
        acetone(&repo, &["put-node", "Host", "a", "--prop", "x=1"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["commit", "-m", "one host"])
            .status
            .success()
    );
    let first = {
        let out = acetone(&repo, &["log"]);
        stdout(&out)
            .lines()
            .next()
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_string()
    };
    assert!(
        acetone(&repo, &["put-node", "Host", "b", "--prop", "x=2"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["commit", "-m", "two hosts"])
            .status
            .success()
    );

    // AT the first commit: one host, in a query whose base is current.
    let out = acetone(
        &repo,
        &[
            "query",
            &format!("MATCH (h:Host) AT '{first}' RETURN count(*) AS n"),
            "--format",
            "csv",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "n\n1");

    // No AT: the base (current) version has two.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN count(*) AS n",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "n\n2");

    // Unresolvable AT ref: a clean error, not a panic.
    let out = acetone(&repo, &["query", "MATCH (h) AT 'nope' RETURN h"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("cannot resolve"), "{}", stderr(&out));
}

#[test]
fn cypher_write_path_persists_and_stays_consistent() {
    // The Phase 3 loop: declare schema, edit via Cypher, read back in fresh
    // processes (persistence), commit, and fsck (edges_rev/index
    // consistency — the mex.2 acceptance).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    for args in [
        &["declare-label", "Host", "--key", "name"][..],
        &["declare-label", "Software", "--key", "name"][..],
        &["declare-rel-type", "RUNS"][..],
    ] {
        let out = acetone(&repo, args);
        assert!(out.status.success(), "{}", stderr(&out));
    }

    // CREATE a node graph.
    let out = acetone(
        &repo,
        &[
            "query",
            "CREATE (a:Host {name: 'web-01', os: 'debian'})-[:RUNS]->(s:Software {name: 'nginx'})",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("2 nodes created, 1 relationship created"));

    // Read it back in a fresh process — proof it persisted.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host)-[:RUNS]->(x:Software) RETURN h.os AS os, x.name AS sw",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "os,sw\ndebian,nginx");

    // MERGE is idempotent: the existing node matches, none created.
    let out = acetone(&repo, &["query", "MERGE (h:Host {name: 'web-01'})"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("(no changes)"));
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN count(h) AS n",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "n\n1");

    // SET then commit; the change survives and the branch advances.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host {name: 'web-01'}) SET h.os = 'ubuntu'",
        ],
    );
    assert!(stdout(&out).contains("1 property set"));
    assert!(
        acetone(&repo, &["commit", "-m", "seed via cypher"])
            .status
            .success()
    );
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN h.os AS os",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "os\nubuntu");

    // DETACH DELETE the whole graph (every node and its edges).
    let out = acetone(&repo, &["query", "MATCH (n) DETACH DELETE n"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("2 nodes deleted"));
    assert!(stdout(&out).contains("1 relationship deleted"));
    let out = acetone(
        &repo,
        &["query", "MATCH (n) RETURN count(n) AS n", "--format", "csv"],
    );
    assert_eq!(stdout(&out).trim(), "n\n0");
    // The relationship went too — edges_rev must stay consistent.
    let out = acetone(&repo, &["fsck"]);
    assert!(out.status.success(), "fsck after writes: {}", stderr(&out));
    assert!(stdout(&out).contains("fsck: clean"));
}

#[test]
fn cypher_write_handles_composite_keys_and_value_types() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    // A composite (two-column) key and varied property types.
    let out = acetone(
        &repo,
        &["declare-label", "Reading", "--key", "sensor", "--key", "at"],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let out = acetone(
        &repo,
        &[
            "query",
            "CREATE (:Reading {sensor: 'temp', at: 1720000000, celsius: 21.5, ok: true, tags: ['a', 'b']})",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("1 node created"));

    // Read back by matching on the composite key, in a fresh process.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (r:Reading {sensor: 'temp', at: 1720000000}) \
             RETURN r.celsius AS c, r.ok AS ok, size(r.tags) AS n",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "c,ok,n\n21.5,true,2");
    assert!(acetone(&repo, &["fsck"]).status.success());
}

#[test]
fn cypher_writes_enforce_schema_constraints() {
    // mex.3: identity, existence and UNIQUE constraints (spec §2,
    // Invariant #3) are enforced on the write path.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let out = acetone(
        &repo,
        &[
            "declare-label",
            "Host",
            "--key",
            "name",
            "--require",
            "os",
            "--unique",
            "ip",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // A well-formed node persists.
    let out = acetone(
        &repo,
        &[
            "query",
            "CREATE (:Host {name: 'a', os: 'debian', ip: '10.0.0.1'})",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // CREATE of an existing key is a conflict (Invariant #3); MERGE upserts.
    let out = acetone(
        &repo,
        &[
            "query",
            "CREATE (:Host {name: 'a', os: 'x', ip: '10.0.0.9'})",
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("conflicts with an existing node"),
        "{}",
        stderr(&out)
    );
    let out = acetone(&repo, &["query", "MERGE (h:Host {name: 'a'})"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // Missing a required property.
    let out = acetone(
        &repo,
        &["query", "CREATE (:Host {name: 'b', ip: '10.0.0.2'})"],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("missing required property"),
        "{}",
        stderr(&out)
    );

    // UNIQUE violation on a non-key property.
    let out = acetone(
        &repo,
        &[
            "query",
            "CREATE (:Host {name: 'c', os: 'y', ip: '10.0.0.1'})",
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("UNIQUE constraint"),
        "{}",
        stderr(&out)
    );

    // Key immutability, caught at persist even when the bind-time gate
    // cannot (an unlabelled MATCH target).
    let out = acetone(&repo, &["query", "MATCH (n) SET n.name = 'renamed'"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("must not change the key property"),
        "{}",
        stderr(&out)
    );

    // None of the failed writes touched the graph: still one host.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN count(h) AS n",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "n\n1");
    assert!(acetone(&repo, &["fsck"]).status.success());

    // Delete-plus-create in one statement (the sanctioned rekey path): the
    // deleted node's key and UNIQUE value are freed within the transaction,
    // so re-using them is not a false conflict.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (n:Host {name:'a'}) DELETE n \
             CREATE (:Host {name:'a', os:'x', ip:'10.0.0.1'})",
        ],
    );
    assert!(
        out.status.success(),
        "delete-plus-create must not false-conflict: {}",
        stderr(&out)
    );
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN h.os AS os",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "os\nx");
    assert!(acetone(&repo, &["fsck"]).status.success());
}

#[test]
fn diff_command_shows_classified_changes() {
    // acetone-14c.1: the graph-level diff between two versions.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    for args in [
        &["declare-label", "Host", "--key", "name"][..],
        &["declare-rel-type", "RUNS"][..],
    ] {
        assert!(acetone(&repo, args).status.success());
    }
    assert!(
        acetone(
            &repo,
            &[
                "query",
                "CREATE (:Host {name:'a'})-[:RUNS]->(:Host {name:'b'})"
            ],
        )
        .status
        .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "v1"]).status.success());
    let log = acetone(&repo, &["log"]);
    let v1 = stdout(&log)
        .lines()
        .next()
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .to_string();

    assert!(
        acetone(
            &repo,
            &["query", "MATCH (h:Host {name:'a'}) SET h.os = 'linux'"]
        )
        .status
        .success()
    );
    assert!(
        acetone(&repo, &["query", "CREATE (:Host {name:'c'})"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "v2"]).status.success());

    let out = acetone(&repo, &["diff", &v1, "main"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("~ node \"Host\" [\"a\"]"), "modified: {text}");
    assert!(text.contains("+ node \"Host\" [\"c\"]"), "added: {text}");
    // The unchanged edge and node b do not appear.
    assert!(!text.contains("\"b\""), "b unchanged: {text}");

    // A version against itself: no changes.
    let out = acetone(&repo, &["diff", "main", "main"]);
    assert_eq!(stdout(&out).trim(), "(no changes)");
}

#[test]
fn rekey_command_changes_a_nodes_identity() {
    // mex.4: a key change is a delete-plus-create in one commit; SET cannot
    // change a key, rekey is the sanctioned path.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    for args in [
        &["declare-label", "Host", "--key", "name"][..],
        &["declare-label", "Software", "--key", "name"][..],
        &["declare-rel-type", "RUNS"][..],
    ] {
        assert!(acetone(&repo, args).status.success());
    }
    let out = acetone(
        &repo,
        &[
            "query",
            "CREATE (:Host {name:'old-01', os:'debian'})-[:RUNS]->(:Software {name:'nginx'})",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(acetone(&repo, &["commit", "-m", "seed"]).status.success());

    // Rekey.
    let out = acetone(
        &repo,
        &["rekey", "Host", "old-01", "web-01", "-m", "rename"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("rekeyed"));

    // Old key gone, new key carries the property and the edge.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (h:Host) RETURN h.name AS n, h.os AS os",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "n,os\nweb-01,debian");
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (:Host {name:'web-01'})-[:RUNS]->(s) RETURN s.name AS s",
            "--format",
            "csv",
        ],
    );
    assert_eq!(stdout(&out).trim(), "s\nnginx");
    assert!(acetone(&repo, &["fsck"]).status.success());
}

#[test]
fn a_write_that_cannot_persist_leaves_the_workspace_untouched() {
    // A CREATE whose node identity cannot be derived (no declared key)
    // fails, and the workspace is not partially advanced (atomicity).
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    // Schemaless repo: CREATE cannot derive identity.
    let out = acetone(&repo, &["query", "CREATE (a:Host {name: 'x'})"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("none of the labels") && stderr(&out).contains("declares a key"),
        "{}",
        stderr(&out)
    );
    // Workspace unchanged: still empty and clean.
    let out = acetone(&repo, &["status"]);
    assert!(stdout(&out).contains("workspace: clean"));
    assert!(stdout(&out).contains("nodes: 0, edges: 0"));
}

/// The "committed <hex>" line's hash.
fn commit_hex(output: &Output) -> String {
    stdout(output)
        .lines()
        .next()
        .unwrap()
        .strip_prefix("committed ")
        .expect("commit line")
        .trim()
        .to_string()
}

#[test]
fn call_history_procedures_run_through_the_query_path() {
    // acetone-8c3: CALL acetone.log/diff execute via the shared provider
    // seam, reading the same Repository history the CLI commands do.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["declare-label", "N", "--key", "id"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["put-node", "N", "1"]).status.success());
    let c1 = commit_hex(&acetone(&repo, &["commit", "-m", "first"]));
    assert!(acetone(&repo, &["put-node", "N", "2"]).status.success());
    let c2 = commit_hex(&acetone(&repo, &["commit", "-m", "second"]));

    // Standalone CALL with no YIELD projects the declared columns.
    let out = acetone(&repo, &["query", "CALL acetone.log()", "--format", "csv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("commit,subject"), "{text}");
    assert!(text.contains("second") && text.contains("first"), "{text}");

    // CALL acetone.diff reads the same Repository::diff as `acetone diff`:
    // node 2 was added between the two commits.
    let query =
        format!("CALL acetone.diff('{c1}', '{c2}') YIELD kind, key RETURN kind, key ORDER BY key");
    let out = acetone(&repo, &["query", &query, "--format", "csv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("added"), "{text}");
    assert!(text.contains("[2]"), "{text}");

    // acetone.conflicts() runs cleanly and yields nothing when no merge is
    // in progress.
    let out = acetone(
        &repo,
        &["query", "CALL acetone.conflicts() YIELD key RETURN key"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("0 rows"), "{}", stdout(&out));
}

#[test]
fn call_diff_yields_a_queryable_virtual_graph() {
    // acetone-14c.1: CALL acetone.diff YIELD node returns the changed nodes as
    // virtual values labelled _Added/_Removed/_Modified, queryable in Cypher.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["declare-label", "N", "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=alice"])
            .status
            .success()
    );
    let c1 = commit_hex(&acetone(&repo, &["commit", "-m", "first"]));
    assert!(
        acetone(&repo, &["put-node", "N", "2", "--prop", "name=bob"])
            .status
            .success()
    );
    let c2 = commit_hex(&acetone(&repo, &["commit", "-m", "second"]));

    // The added node is a virtual :_Added node carrying its real label and
    // both key (id) and record (name) properties.
    let query = format!(
        "CALL acetone.diff('{c1}', '{c2}') YIELD node \
         WHERE '_Added' IN labels(node) RETURN node.id AS id, node.name AS name"
    );
    let out = acetone(&repo, &["query", &query, "--format", "csv"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("id,name"), "{text}");
    assert!(text.contains("2,bob"), "{text}");
    // The unchanged/added node 1 is not _Added here (only 2 was added).
    assert!(!text.contains("1,alice"), "{text}");
}

#[test]
fn call_blame_attributes_node_changes_to_commits() {
    // acetone-14c.6: CALL acetone.blame(label, key) lists the commits that
    // changed a node, newest first, skipping unrelated commits.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["declare-label", "N", "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=alice"])
            .status
            .success()
    );
    let c1 = commit_hex(&acetone(&repo, &["commit", "-m", "add 1"]));
    assert!(acetone(&repo, &["put-node", "N", "2"]).status.success());
    let _c2 = commit_hex(&acetone(&repo, &["commit", "-m", "add 2"]));
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=alice2"])
            .status
            .success()
    );
    let c3 = commit_hex(&acetone(&repo, &["commit", "-m", "rename 1"]));

    let out = acetone(
        &repo,
        &[
            "query",
            "CALL acetone.blame('N', 1) YIELD commit RETURN commit",
            "--format",
            "csv",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    // Both the rename (c3) and the add (c1) touched node 1; c2 did not.
    assert!(text.contains(&c3), "{text}");
    assert!(text.contains(&c1), "{text}");
    // c3 (newest) appears before c1.
    let pos3 = text.find(&c3).unwrap();
    let pos1 = text.find(&c1).unwrap();
    assert!(pos3 < pos1, "blame must be newest-first: {text}");
}

#[test]
fn merge_conflict_resolve_and_complete() {
    // acetone-14c.4a: a conflicted merge enters merge-in-progress; resolve
    // picks a side; commit completes it as a two-parent merge.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["declare-label", "N", "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=base"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "base"]).status.success());
    assert!(acetone(&repo, &["branch", "other"]).status.success());
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=ours"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "ours"]).status.success());
    assert!(acetone(&repo, &["checkout", "other"]).status.success());
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=theirs"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "theirs"]).status.success());
    assert!(acetone(&repo, &["checkout", "main"]).status.success());

    // The merge conflicts and exits non-zero, entering merge-in-progress.
    let out = acetone(&repo, &["merge", "other", "-m", "merge"]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("1 conflict"), "{}", stdout(&out));

    // Status reports the in-progress merge; commit refuses.
    assert!(stdout(&acetone(&repo, &["status"])).contains("merge: in progress"));
    let premature = acetone(&repo, &["commit", "-m", "no"]);
    assert!(!premature.status.success());
    assert!(stderr(&premature).contains("unresolved conflict"));

    // Resolve to theirs, then commit completes the merge.
    assert!(
        acetone(&repo, &["resolve", "--all-theirs"])
            .status
            .success()
    );
    let done = acetone(&repo, &["commit", "-m", "merge done"]);
    assert!(done.status.success(), "{}", stderr(&done));

    // The chosen value survived; the workspace is clean and fsck passes.
    let q = acetone(
        &repo,
        &[
            "query",
            "MATCH (n:N) RETURN n.name AS name",
            "--format",
            "csv",
        ],
    );
    assert!(stdout(&q).contains("theirs"), "{}", stdout(&q));
    assert!(stdout(&acetone(&repo, &["status"])).contains("workspace: clean"));
    assert!(acetone(&repo, &["fsck"]).status.success());
}

#[test]
fn call_conflicts_exposes_the_merge_conflicts() {
    // acetone-14c.4b: CALL acetone.conflicts() yields the conflicting entities
    // and the _Conflict virtual subgraph during a merge in progress.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["declare-label", "N", "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=base"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "base"]).status.success());
    assert!(acetone(&repo, &["branch", "other"]).status.success());
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=ours"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "ours"]).status.success());
    assert!(acetone(&repo, &["checkout", "other"]).status.success());
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=theirs"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "theirs"]).status.success());
    assert!(acetone(&repo, &["checkout", "main"]).status.success());
    let _ = acetone(&repo, &["merge", "other", "-m", "merge"]); // conflicts

    // The conflict is reported with its label and key.
    let rows = acetone(
        &repo,
        &[
            "query",
            "CALL acetone.conflicts() YIELD label, key RETURN label, key",
            "--format",
            "csv",
        ],
    );
    assert!(stdout(&rows).contains("N"), "{}", stdout(&rows));

    // The _Conflict virtual node carries ours' value.
    let node = acetone(
        &repo,
        &[
            "query",
            "CALL acetone.conflicts() YIELD node \
             WHERE '_Conflict' IN labels(node) RETURN node.name AS name",
            "--format",
            "csv",
        ],
    );
    assert!(stdout(&node).contains("ours"), "{}", stdout(&node));

    // After completing the merge, there are no conflicts.
    assert!(acetone(&repo, &["resolve", "--all-ours"]).status.success());
    assert!(acetone(&repo, &["commit", "-m", "done"]).status.success());
    let none = acetone(
        &repo,
        &[
            "query",
            "CALL acetone.conflicts() YIELD label RETURN count(*) AS n",
            "--format",
            "csv",
        ],
    );
    assert!(stdout(&none).contains("\n0"), "{}", stdout(&none));
}

#[test]
fn conflicts_node_falls_back_to_theirs_when_ours_deleted() {
    // acetone-14c.4b: the _Conflict node shows ours' value, but when ours
    // deleted the node (delete-vs-modify), it falls back to theirs'.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["declare-label", "N", "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=base"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "base"]).status.success());
    assert!(acetone(&repo, &["branch", "other"]).status.success());
    // theirs modifies the node.
    assert!(acetone(&repo, &["checkout", "other"]).status.success());
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=theirs"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "theirs"]).status.success());
    // ours deletes the node.
    assert!(acetone(&repo, &["checkout", "main"]).status.success());
    assert!(
        acetone(&repo, &["query", "MATCH (n:N {id: 1}) DELETE n"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["commit", "-m", "ours deletes"])
            .status
            .success()
    );
    let _ = acetone(&repo, &["merge", "other", "-m", "merge"]); // conflicts

    // ours has no node, so the _Conflict node carries theirs' value.
    let out = acetone(
        &repo,
        &[
            "query",
            "CALL acetone.conflicts() YIELD node \
             WHERE '_Conflict' IN labels(node) RETURN node.name AS name",
            "--format",
            "csv",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("theirs"), "{}", stdout(&out));
}

#[test]
fn merge_conflict_resolved_by_ordinary_write() {
    // acetone-14c.4c: a conflict can be resolved by writing a custom merged
    // value (not just picking a side), then commit completes the merge.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["declare-label", "N", "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=base"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "base"]).status.success());
    assert!(acetone(&repo, &["branch", "other"]).status.success());
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=ours"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "ours"]).status.success());
    assert!(acetone(&repo, &["checkout", "other"]).status.success());
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=theirs"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "theirs"]).status.success());
    assert!(acetone(&repo, &["checkout", "main"]).status.success());
    let _ = acetone(&repo, &["merge", "other", "-m", "merge"]);

    // Resolve by writing a hand-merged value.
    assert!(
        acetone(&repo, &["put-node", "N", "1", "--prop", "name=merged"])
            .status
            .success()
    );
    assert!(stdout(&acetone(&repo, &["status"])).contains("all conflicts resolved"));
    assert!(acetone(&repo, &["commit", "-m", "done"]).status.success());
    let q = acetone(
        &repo,
        &[
            "query",
            "MATCH (n:N) RETURN n.name AS name",
            "--format",
            "csv",
        ],
    );
    assert!(stdout(&q).contains("merged"), "{}", stdout(&q));
}

/// acetone-c8b: the shell must run write queries through the transactional
/// write path (advancing the workspace), not silently execute only the read
/// side and discard the mutation.
#[test]
fn shell_persists_write_queries() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    // A keyed label so a Cypher SET can identify the node (Invariant #3).
    let out = acetone(&repo, &["declare-label", "Host", "--key", "name"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = acetone(&repo, &["put-node", "Host", "web1", "--prop", "os=debian"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // Drive the shell over stdin: a SET write, then EOF (stdin closed).
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut child = std::process::Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "shell"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn acetone shell");
    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin
            .write_all(b"MATCH (h:Host {name: 'web1'}) SET h.os = 'ubuntu';\n")
            .expect("write query");
    } // stdin dropped -> EOF -> shell exits
    let shell_out = child.wait_with_output().expect("wait");
    assert!(
        shell_out.status.success(),
        "shell exit: {}",
        String::from_utf8_lossy(&shell_out.stderr)
    );

    // The write must have advanced the workspace: a fresh read sees 'ubuntu'.
    let out = acetone(&repo, &["query", "MATCH (h:Host) RETURN h.os"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("ubuntu"),
        "shell write was discarded; query output: {text}"
    );
    assert!(!text.contains("debian"), "old value still present: {text}");
}

/// acetone-8yn (U11): CREATE must not silently overwrite an existing edge or
/// collapse parallel edges (v0.1 has no query-reachable discriminator, ADR-0030).
/// MERGE (upsert) and SET (modify) on an existing edge still work.
#[test]
fn create_of_a_duplicate_edge_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    assert!(
        acetone(&repo, &["declare-label", "Host", "--key", "name"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["declare-rel-type", "RUNS"])
            .status
            .success()
    );

    // Create two nodes and an edge between them.
    let out = acetone(
        &repo,
        &[
            "query",
            "CREATE (a:Host {name:'a'})-[:RUNS]->(b:Host {name:'b'})",
        ],
    );
    assert!(out.status.success(), "initial create: {}", stderr(&out));

    // Re-creating the same edge over the matched nodes is rejected.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (a:Host {name:'a'}), (b:Host {name:'b'}) CREATE (a)-[:RUNS]->(b)",
        ],
    );
    assert!(
        !out.status.success(),
        "duplicate edge CREATE must fail, but it succeeded"
    );
    assert!(
        stderr(&out).contains("existing relationship"),
        "expected a duplicate-edge error, got: {}",
        stderr(&out)
    );

    // Two parallel CREATEs in one statement collapse — also rejected.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (a:Host {name:'a'}), (b:Host {name:'b'}) \
             CREATE (a)-[:RUNS]->(b), (a)-[:RUNS]->(b)",
        ],
    );
    assert!(!out.status.success(), "parallel edge CREATE must fail");

    // MERGE of the existing edge is an upsert (matches) — still works.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (a:Host {name:'a'}), (b:Host {name:'b'}) MERGE (a)-[:RUNS]->(b)",
        ],
    );
    assert!(
        out.status.success(),
        "MERGE of an existing edge must succeed: {}",
        stderr(&out)
    );

    // SET on the matched edge modifies it — still works.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (a:Host {name:'a'})-[r:RUNS]->(b:Host {name:'b'}) SET r.weight = 5",
        ],
    );
    assert!(
        out.status.success(),
        "SET on a matched edge must succeed: {}",
        stderr(&out)
    );

    // DELETE then re-CREATE the same edge in one statement frees the key — allowed.
    let out = acetone(
        &repo,
        &[
            "query",
            "MATCH (a:Host {name:'a'})-[r:RUNS]->(b:Host {name:'b'}) \
             DELETE r CREATE (a)-[:RUNS]->(b)",
        ],
    );
    assert!(
        out.status.success(),
        "delete-then-recreate must succeed: {}",
        stderr(&out)
    );

    // A self-loop is a distinct edge and creates fine; a duplicate self-loop is
    // rejected like any other duplicate.
    let out = acetone(
        &repo,
        &["query", "MATCH (a:Host {name:'a'}) CREATE (a)-[:RUNS]->(a)"],
    );
    assert!(out.status.success(), "self-loop create: {}", stderr(&out));
    let out = acetone(
        &repo,
        &["query", "MATCH (a:Host {name:'a'}) CREATE (a)-[:RUNS]->(a)"],
    );
    assert!(
        !out.status.success(),
        "duplicate self-loop must be rejected"
    );
}

/// acetone-do1: --version must report the real crate version, not the 0.0.1
/// placeholder (the tagged binary must not claim to be 0.0.1).
#[test]
fn version_flag_reports_the_crate_version() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_acetone"))
        .arg("--version")
        .output()
        .expect("run --version");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(env!("CARGO_PKG_VERSION")),
        "--version output {text:?} should contain the crate version {}",
        env!("CARGO_PKG_VERSION")
    );
    assert!(
        !text.contains("0.0.1"),
        "--version must not report the 0.0.1 placeholder: {text:?}"
    );
}

/// acetone-do1: help text must not contradict the shipped behaviour (merge
/// conflict resolution exists; the shell has no :diff).
#[test]
fn help_text_matches_shipped_behaviour() {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let merge = std::process::Command::new(bin)
        .args(["merge", "--help"])
        .output()
        .expect("merge --help");
    let merge_text = String::from_utf8_lossy(&merge.stdout);
    assert!(
        !merge_text.contains("not yet available"),
        "merge --help still claims conflict resolution is unavailable"
    );
    assert!(
        merge_text.contains("resolve"),
        "merge --help should mention `acetone resolve`"
    );

    let shell = std::process::Command::new(bin)
        .args(["shell", "--help"])
        .output()
        .expect("shell --help");
    assert!(
        !String::from_utf8_lossy(&shell.stdout).contains(":diff"),
        "shell --help still promises a :diff that does not exist"
    );
}

/// A no-flag `migrate` must re-chunk under the repo's *current* parameters, so
/// it leaves every commit hash unchanged (history-independence) rather than
/// silently imposing a different chunk profile. Regression test for the
/// migrate-defaults foot-gun (acetone-7bn.7 review).
#[test]
fn migrate_with_no_flags_preserves_history() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path();
    assert!(init(repo).status.success(), "init");
    assert!(
        acetone(repo, &["put-node", "Host", "web-01", "--prop", "os=linux"])
            .status
            .success(),
        "put-node"
    );
    assert!(
        acetone(repo, &["commit", "-m", "seed"]).status.success(),
        "commit"
    );

    let log_before = stdout(&acetone(repo, &["log"]));

    let migrated = acetone(repo, &["migrate"]);
    assert!(migrated.status.success(), "no-flag migrate should succeed");

    let log_after = stdout(&acetone(repo, &["log"]));
    assert_eq!(
        log_before, log_after,
        "no-flag migrate changed the history/hashes; it must preserve the current chunk profile"
    );
    // And integrity still holds.
    assert!(
        acetone(repo, &["fsck"]).status.success(),
        "fsck after no-flag migrate"
    );
}

/// `acetone schema` dumps the declared schema grouped into labels,
/// relationship types and indexes, and `--at <ref>` reads a past version's
/// schema without checking it out (acetone-7bn.10).
#[test]
fn schema_command_shows_and_time_travels() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path();
    assert!(init(repo).status.success(), "init");

    // Empty case: a fresh repo has no declared schema.
    let out = acetone(repo, &["schema"]);
    assert!(out.status.success(), "schema (empty): {}", stderr(&out));
    assert!(
        stdout(&out).contains("(no schema declared)"),
        "empty schema should say so, got:\n{}",
        stdout(&out)
    );

    // Declare a label with key/require/unique constraints, a relationship
    // type, and an index; commit them.
    assert!(
        acetone(
            repo,
            &[
                "declare-label",
                "Host",
                "--key",
                "hostname",
                "--require",
                "os",
                "--unique",
                "mac",
            ],
        )
        .status
        .success(),
        "declare-label Host"
    );
    assert!(
        acetone(repo, &["declare-rel-type", "RUNS"])
            .status
            .success(),
        "declare-rel-type RUNS"
    );
    assert!(
        acetone(
            repo,
            &[
                "declare-index",
                "by_host",
                "--label",
                "Host",
                "--property",
                "hostname",
            ],
        )
        .status
        .success(),
        "declare-index by_host"
    );
    assert!(
        acetone(repo, &["commit", "-m", "schema v1"])
            .status
            .success(),
        "commit schema v1"
    );

    // The first commit's hash — the ref we will time-travel back to.
    let first_commit = stdout(&acetone(repo, &["log"]))
        .lines()
        .next()
        .expect("a commit in the log")
        .split_whitespace()
        .next()
        .expect("a hash")
        .to_owned();

    // Populated case: names the label, its key, the constraints, the type
    // and the index.
    let out = acetone(repo, &["schema"]);
    assert!(out.status.success(), "schema (populated): {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Labels"), "has a Labels heading:\n{text}");
    assert!(text.contains("\"Host\""), "names Host:\n{text}");
    assert!(text.contains("\"hostname\""), "shows the key:\n{text}");
    assert!(
        text.contains("\"os\""),
        "shows the required property:\n{text}"
    );
    assert!(
        text.contains("\"mac\""),
        "shows the unique property:\n{text}"
    );
    assert!(
        text.contains("Relationship types"),
        "has a Relationship types heading:\n{text}"
    );
    assert!(text.contains("\"RUNS\""), "names RUNS:\n{text}");
    assert!(text.contains("Indexes"), "has an Indexes heading:\n{text}");
    assert!(text.contains("\"by_host\""), "names the index:\n{text}");

    // Change the schema and commit again: add a second relationship type.
    assert!(
        acetone(repo, &["declare-rel-type", "DEPENDS_ON"])
            .status
            .success(),
        "declare-rel-type DEPENDS_ON"
    );
    assert!(
        acetone(repo, &["commit", "-m", "schema v2"])
            .status
            .success(),
        "commit schema v2"
    );

    // The current workspace schema now includes the new type.
    let now = stdout(&acetone(repo, &["schema"]));
    assert!(
        now.contains("\"DEPENDS_ON\""),
        "current schema includes the new type:\n{now}"
    );

    // `--at` the first commit reads the EARLIER schema — without a checkout —
    // so the later type is absent there.
    let out = acetone(repo, &["schema", "--at", &first_commit]);
    assert!(out.status.success(), "schema --at: {}", stderr(&out));
    let past = stdout(&out);
    assert!(
        past.contains("\"RUNS\""),
        "past schema still has RUNS:\n{past}"
    );
    assert!(
        !past.contains("\"DEPENDS_ON\""),
        "past schema must not show the later type:\n{past}"
    );
}

// --- `--json` machine output (acetone-7bn.11) --------------------------------
//
// Every case parses stdout with serde_json to prove it is valid JSON, then
// asserts on the parsed value — never on brittle formatting. The JSON *shape*
// is explicitly unstable at 0.1.1 (may change before 0.2); these tests pin
// current behaviour, not a frozen contract.

use serde_json::Value as Json;

/// Parse a command's stdout as JSON, failing loudly if it is not valid.
fn json_stdout(output: &Output) -> Json {
    let text = stdout(output);
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\n{text}"))
}

/// A seeded repository for the read-command JSON tests: a keyed `Host` label
/// with a required and a unique constraint, an index, one relationship type,
/// and a small committed graph exercising several value kinds.
fn seed_json_repo(repo: &Path) {
    assert!(init(repo).status.success());
    for args in [
        &[
            "declare-label",
            "Host",
            "--key",
            "name",
            "--require",
            "os",
            "--unique",
            "ip",
        ][..],
        &["declare-rel-type", "RUNS"][..],
        &[
            "declare-index",
            "host_by_os",
            "--label",
            "Host",
            "--property",
            "os",
        ][..],
    ] {
        assert!(
            acetone(repo, args).status.success(),
            "{}",
            stderr(&acetone(repo, args))
        );
    }
    let out = acetone(
        repo,
        &[
            "query",
            "CREATE (:Host {name:'web1', os:'linux', ip:'10.0.0.1', up:true, load:0.5, tags:['a','b']})",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(acetone(repo, &["commit", "-m", "seed"]).status.success());
}

#[test]
fn status_json_reports_the_workspace_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    seed_json_repo(&repo);

    let out = acetone(&repo, &["status", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_stdout(&out);
    assert_eq!(v["branch"], "main");
    assert_eq!(v["workspace"], "clean");
    assert_eq!(v["nodes"], 1);
    assert_eq!(v["edges"], 0);
    assert_eq!(v["schema_entries"], 3);
    assert!(v["head"].is_string(), "head is a hash string: {v}");
    assert!(v["merge"].is_null(), "no merge in progress: {v}");
}

#[test]
fn schema_json_lists_labels_types_and_indexes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    seed_json_repo(&repo);

    let out = acetone(&repo, &["schema", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_stdout(&out);

    let label = &v["labels"][0];
    assert_eq!(label["name"], "Host");
    assert_eq!(label["key"], serde_json::json!(["name"]));
    assert_eq!(label["required"], serde_json::json!(["os"]));
    assert_eq!(label["unique"], serde_json::json!(["ip"]));
    assert_eq!(label["surrogate"], false);
    assert_eq!(v["relationship_types"], serde_json::json!(["RUNS"]));
    let index = &v["indexes"][0];
    assert_eq!(index["name"], "host_by_os");
    assert_eq!(index["label"], "Host");
    assert_eq!(index["properties"], serde_json::json!(["os"]));
}

#[test]
fn get_node_json_hit_returns_the_node_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    seed_json_repo(&repo);

    let out = acetone(&repo, &["get-node", "Host", "web1", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_stdout(&out);
    assert_eq!(v["label"], "Host");
    assert_eq!(v["key"], serde_json::json!(["web1"]));
    assert_eq!(v["secondary_labels"], serde_json::json!([]));
    // Value kinds map to their natural JSON forms.
    assert_eq!(v["properties"]["os"], "linux");
    assert_eq!(v["properties"]["up"], true);
    assert_eq!(v["properties"]["load"], 0.5);
    assert_eq!(v["properties"]["tags"], serde_json::json!(["a", "b"]));
}

#[test]
fn get_node_json_miss_is_null_on_stdout_and_nonzero_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    seed_json_repo(&repo);

    let out = acetone(&repo, &["get-node", "Host", "absent", "--json"]);
    // Non-zero exit so a script can still detect the miss by status code…
    assert!(!out.status.success(), "a miss must exit non-zero");
    // …while stdout parses as JSON `null`.
    assert_eq!(json_stdout(&out), Json::Null);
    assert!(stderr(&out).contains("not found"));
}

#[test]
fn list_nodes_json_is_an_array_of_node_objects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    seed_json_repo(&repo);
    assert!(
        acetone(
            &repo,
            &["put-node", "Place", "paris", "--prop", "country=fr"]
        )
        .status
        .success()
    );

    let out = acetone(&repo, &["list-nodes", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_stdout(&out);
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 2, "two nodes: {v}");

    // The `--label` filter narrows the array.
    let out = acetone(&repo, &["list-nodes", "--label", "Host", "--json"]);
    let v = json_stdout(&out);
    let arr = v.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["label"], "Host");
    assert_eq!(arr[0]["key"], serde_json::json!(["web1"]));
}

#[test]
fn log_and_branch_and_diff_json_shapes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    seed_json_repo(&repo);
    let v1 = commit_hex_from_log(&repo);

    // A second commit so diff has changes.
    assert!(
        acetone(
            &repo,
            &["query", "MATCH (h:Host {name:'web1'}) SET h.os='ubuntu'"]
        )
        .status
        .success()
    );
    assert!(
        acetone(&repo, &["commit", "-m", "update os"])
            .status
            .success()
    );

    // log: array of commit objects, newest first.
    let out = acetone(&repo, &["log", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_stdout(&out);
    let entries = v.as_array().expect("array");
    assert_eq!(entries.len(), 2, "two commits: {v}");
    assert!(entries[0]["hash"].is_string());
    assert_eq!(entries[0]["subject"], "update os");
    assert!(entries[0]["trailers"].is_array());
    assert!(entries[0]["parents"].is_array());

    // branch (list): current plus the branch names.
    assert!(acetone(&repo, &["branch", "feature"]).status.success());
    let out = acetone(&repo, &["branch", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_stdout(&out);
    assert_eq!(v["current"], "main");
    let names = v["branches"].as_array().expect("array");
    assert!(names.iter().any(|n| n == "main"));
    assert!(names.iter().any(|n| n == "feature"));

    // diff: from/to plus a changes array mirroring the human diff.
    let out = acetone(&repo, &["diff", &v1, "main", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = json_stdout(&out);
    assert_eq!(v["from"], v1);
    assert_eq!(v["to"], "main");
    let changes = v["changes"].as_array().expect("array");
    assert!(
        changes
            .iter()
            .any(|c| c["kind"] == "node_modified" && c["key"] == serde_json::json!(["web1"])),
        "web1 modified: {v}"
    );
}

/// Hostile property values are escaped by serde_json (never raw ANSI/C1),
/// the same bar the human paths meet with `sanitise_line`.
#[test]
fn json_output_escapes_hostile_property_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    // ESC (C0), DEL (0x7f) and a C1 control (0x9b) in a property value.
    assert!(
        acetone(
            &repo,
            &[
                "put-node",
                "Host",
                "1",
                "--prop",
                "note=ok\u{1b}[31m\u{7f}\u{9b}m"
            ],
        )
        .status
        .success()
    );

    let out = acetone(&repo, &["get-node", "Host", "1", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        !text.contains('\u{1b}'),
        "raw ESC reached JSON output: {text:?}"
    );
    assert!(!text.contains('\u{7f}'), "raw DEL reached JSON output");
    assert!(!text.contains('\u{9b}'), "raw C1 CSI reached JSON output");
    // Still valid JSON that parses back to the original bytes.
    let v = json_stdout(&out);
    assert_eq!(v["properties"]["note"], "ok\u{1b}[31m\u{7f}\u{9b}m");
}

/// The `commit_hex` helper reads the "committed <hex>" line; this reads the
/// first (newest) commit hash out of `log`.
fn commit_hex_from_log(repo: &Path) -> String {
    let out = acetone(repo, &["log"]);
    stdout(&out)
        .lines()
        .next()
        .unwrap()
        .split(' ')
        .next()
        .unwrap()
        .to_string()
}
