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

    // Surgically destroy the repository's objects, sparing only the
    // workspace manifest blob (so `open` still succeeds and fsck itself
    // runs): fsck must report error findings, exit non-zero, no Debug
    // dump. (A random victim won't do — a fresh repo also holds
    // unreachable superseded manifests whose loss fsck rightly ignores.)
    let manifest_oid = git_rev_parse(&repo, "refs/acetone/workspaces/default");
    let spared = repo
        .join("objects")
        .join(&manifest_oid[..2])
        .join(&manifest_oid[2..]);
    for object in loose_objects(&repo.join("objects")) {
        if object != spared {
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
    use acetone_graph::{InitOptions, Repository};
    use acetone_model::Value;
    use acetone_model::graph_keys::NodeKey;
    use acetone_model::records::NodeRecord;

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
            "--prop",
            "note=ok\u{1b}[31mred\u{7}",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    // Table: property value and label must be escaped, not raw.
    let out = acetone(&repo, &["query", "MATCH (n) RETURN n.note AS note, n"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(!text.contains('\u{1b}'), "raw ESC reached table output");
    assert!(!text.contains('\u{7}'), "raw BEL reached table output");
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
    // JSON must also escape DEL and C1 controls (Phase 2 security review
    // MINOR-1: align json_string with sanitise_line's coverage).
    assert!(!text.contains('\u{7}'), "raw BEL reached JSON output");
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
