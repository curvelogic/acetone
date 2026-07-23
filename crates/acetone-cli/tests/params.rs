//! End-to-end tests for query parameter binding (`acetone query --param
//! KEY=VALUE` and the shell's `:param`, bead acetone-9zt): typed values
//! reach the executor as `$name`, malformed bindings fail loudly naming the
//! parameter, and `--param` composes with `--at` time travel.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn acetone(repo: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(bin).args(&full).output().expect("run acetone")
}

fn init(repo: &Path) {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let ok = Command::new(bin)
        .args(["init", repo.to_str().unwrap()])
        .output()
        .expect("init")
        .status
        .success();
    assert!(ok, "init");
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "command failed: {}", stderr(o));
    stdout(o)
}

/// A repo with a declared `Service` label and two committed nodes.
fn seeded_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path());
    ok(&acetone(
        dir.path(),
        &["declare-label", "Service", "--key", "name"],
    ));
    ok(&acetone(
        dir.path(),
        &[
            "query",
            "CREATE (:Service {name: 'billing', replicas: 2}), \
             (:Service {name: 'identity', replicas: 5})",
        ],
    ));
    ok(&acetone(dir.path(), &["commit", "-m", "seed services"]));
    dir
}

#[test]
fn typed_params_bind_and_round_trip() {
    let dir = seeded_repo();
    // JSON output is the unambiguous typing check: an int stays a number,
    // a string stays quoted, booleans and null are JSON's own.
    let s = ok(&acetone(
        dir.path(),
        &[
            "query",
            "RETURN $n AS n, $s AS s, $b AS b, $z AS z",
            "--format",
            "json",
            "--param",
            "n=42",
            "--param",
            "s='billing'",
            "--param",
            "b=true",
            "--param",
            "z=null",
        ],
    ));
    assert!(s.contains("\"n\": 42"), "int param: {s}");
    assert!(s.contains("\"s\": \"billing\""), "string param: {s}");
    assert!(s.contains("\"b\": true"), "bool param: {s}");
    assert!(s.contains("\"z\": null"), "null param: {s}");
}

#[test]
fn params_affect_match_results() {
    let dir = seeded_repo();
    // The parameter drives the filter: only `identity` has replicas > 3.
    let s = ok(&acetone(
        dir.path(),
        &[
            "query",
            "MATCH (s:Service) WHERE s.replicas > $min RETURN s.name",
            "--param",
            "min=3",
            "--format",
            "csv",
        ],
    ));
    assert!(s.contains("identity"), "should match identity: {s}");
    assert!(!s.contains("billing"), "should not match billing: {s}");

    // A string parameter binds a key lookup.
    let s = ok(&acetone(
        dir.path(),
        &[
            "query",
            "MATCH (s:Service {name: $name}) RETURN s.replicas",
            "--param",
            "name=\"billing\"",
            "--format",
            "csv",
        ],
    ));
    assert!(s.contains('2'), "billing has 2 replicas: {s}");
}

#[test]
fn list_param_drives_unwind() {
    let dir = seeded_repo();
    let s = ok(&acetone(
        dir.path(),
        &[
            "query",
            "UNWIND $names AS n MATCH (s:Service {name: n}) RETURN s.name ORDER BY s.name",
            "--param",
            "names=['billing', 'identity']",
            "--format",
            "csv",
        ],
    ));
    assert!(s.contains("billing") && s.contains("identity"), "{s}");
}

#[test]
fn missing_param_still_errors_clearly() {
    let dir = seeded_repo();
    let o = acetone(
        dir.path(),
        &["query", "MATCH (s:Service {name: $name}) RETURN s.name"],
    );
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("missing parameter 'name'"),
        "unexpected error: {}",
        stderr(&o)
    );
}

#[test]
fn malformed_value_errors_name_the_parameter() {
    let dir = seeded_repo();
    // A bare word is not silently a string: the error names the parameter
    // and teaches the quoting idiom.
    let o = acetone(
        dir.path(),
        &["query", "RETURN $name", "--param", "name=billing"],
    );
    assert!(!o.status.success());
    let e = stderr(&o);
    assert!(e.contains("--param name"), "names the parameter: {e}");
    assert!(e.contains("quote strings"), "teaches the fix: {e}");

    // No '=' at all.
    let o = acetone(dir.path(), &["query", "RETURN 1", "--param", "nope"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("KEY=VALUE"), "{}", stderr(&o));

    // An expression is not a literal.
    let o = acetone(dir.path(), &["query", "RETURN $n", "--param", "n=1 + 2"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("--param n"), "{}", stderr(&o));

    // The same parameter bound twice is an error, not a silent override.
    let o = acetone(
        dir.path(),
        &["query", "RETURN $n", "--param", "n=1", "--param", "n=2"],
    );
    assert!(!o.status.success());
    assert!(stderr(&o).contains("bound twice"), "{}", stderr(&o));
}

#[test]
fn key_may_carry_the_dollar_sigil() {
    let dir = seeded_repo();
    // Writing the KEY the way it appears in the query (`--param '$n=…'`)
    // binds `$n`, not the unreachable name `"$n"`.
    let s = ok(&acetone(
        dir.path(),
        &[
            "query",
            "RETURN $n AS n",
            "--format",
            "json",
            "--param",
            "$n=42",
        ],
    ));
    assert!(s.contains("\"n\": 42"), "sigil-prefixed KEY binds: {s}");

    // Exactly one sigil is stripped, so `$n` and `n` are the same binding —
    // giving both is the duplicate error.
    let o = acetone(
        dir.path(),
        &["query", "RETURN $n", "--param", "$n=1", "--param", "n=2"],
    );
    assert!(!o.status.success());
    assert!(stderr(&o).contains("bound twice"), "{}", stderr(&o));

    // A sigil with nothing after it is still a missing name.
    let o = acetone(dir.path(), &["query", "RETURN 1", "--param", "$=1"]);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("missing the parameter name"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn params_work_with_at_time_travel() {
    let dir = seeded_repo();
    // Advance the workspace past the seed commit: rename-by-recreate isn't
    // possible (keys are immutable), so change a non-key property instead.
    ok(&acetone(
        dir.path(),
        &[
            "query",
            "MATCH (s:Service {name: 'billing'}) SET s.replicas = 9",
        ],
    ));
    ok(&acetone(dir.path(), &["commit", "-m", "scale billing"]));

    // At the current head the parameterised lookup sees the new value…
    let s = ok(&acetone(
        dir.path(),
        &[
            "query",
            "MATCH (s:Service {name: $name}) RETURN s.replicas",
            "--param",
            "name=\"billing\"",
            "--at",
            "main",
            "--format",
            "csv",
        ],
    ));
    assert!(s.contains('9'), "head sees the update: {s}");

    // …and at the seed commit (main's parent) it sees the original.
    let log = ok(&acetone(dir.path(), &["log"]));
    let seed_line = log
        .lines()
        .find(|l| l.contains("seed services"))
        .expect("seed commit in log");
    let seed_hash = seed_line.split_whitespace().next().expect("hash");
    let s = ok(&acetone(
        dir.path(),
        &[
            "query",
            "MATCH (s:Service {name: $name}) RETURN s.replicas",
            "--param",
            "name=\"billing\"",
            "--at",
            seed_hash,
            "--format",
            "csv",
        ],
    ));
    assert!(s.contains('2'), "the past is unchanged: {s}");
    assert!(!s.contains('9'), "the past is unchanged: {s}");
}

/// Pipe `input` into `acetone shell` and return its output.
fn shell(repo: &Path, input: &str) -> Output {
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
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait shell")
}

#[test]
fn shell_param_binds_lists_and_clears() {
    let dir = seeded_repo();
    // `:param $name …` — the sigil is accepted here too, binding `name`.
    let o = shell(
        dir.path(),
        ":param $name 'billing'\n\
         :param\n\
         MATCH (s:Service {name: $name}) RETURN s.replicas AS r;\n\
         :param-clear name\n\
         :param\n\
         :quit\n",
    );
    let s = stdout(&o);
    assert!(
        s.contains("$name = billing"),
        "bare :param lists the binding: {s}"
    );
    assert!(s.contains('2'), "the bound query ran: {s}");
    assert!(
        s.contains("(no parameters)"),
        ":param-clear removed it: {s}"
    );

    // After clearing, the parameter is gone — the statement errors again.
    let o = shell(
        dir.path(),
        ":param name 'billing'\n\
         :param-clear\n\
         MATCH (s:Service {name: $name}) RETURN s.replicas;\n\
         :quit\n",
    );
    assert!(
        stderr(&o).contains("missing parameter 'name'"),
        "cleared param no longer binds: {}",
        stderr(&o)
    );

    // A malformed :param literal errors without killing the session.
    let o = shell(
        dir.path(),
        ":param name billing\n\
         RETURN 1 AS still_alive;\n\
         :quit\n",
    );
    assert!(
        stderr(&o).contains("parameter name"),
        "bad literal reported: {}",
        stderr(&o)
    );
    assert!(
        stdout(&o).contains("still_alive"),
        "session survives: {}",
        stdout(&o)
    );
}
