//! Trojan-source hardening (bead acetone-7bn.19): a hostile-clone property
//! value or branch name containing a bidirectional override (U+202E
//! RIGHT-TO-LEFT OVERRIDE) must never reach the terminal raw — on the human
//! table path, the `query --format json` path, the `--json` read-command path,
//! or the `status`/`branch`-list branch-name paths — so it cannot visually
//! reorder what the user sees. The escaped forms round-trip.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_acetone")
}

fn acetone(repo: &Path, args: &[&str]) -> Output {
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(bin())
        .args(&full)
        .output()
        .expect("failed to run acetone")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("stdout is not UTF-8")
}

/// A repository with one Host node whose `os` property hides a right-to-left
/// override between two visible runs.
fn repo_with_hostile_value(dir: &Path) -> String {
    Command::new(bin())
        .args(["init", dir.to_str().unwrap()])
        .output()
        .expect("init");
    assert!(
        acetone(dir, &["declare-label", "Host", "--key", "hostname"])
            .status
            .success()
    );
    // The property value: "safe\u{202e}reversed". Injected via a Cypher
    // literal so it lands as a real property value in the graph.
    let create = "CREATE (:Host {hostname:\"h1\", os:\"safe\u{202e}reversed\"})";
    let out = acetone(dir, &["query", create]);
    assert!(out.status.success(), "create failed: {out:?}");
    "safe\u{202e}reversed".to_string()
}

#[test]
fn override_is_escaped_on_every_output_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    repo_with_hostile_value(dir);

    // 1. Human table path (routes through sanitise_line): escaped as \u{202e}.
    let table = stdout(&acetone(
        dir,
        &["query", "MATCH (h:Host) RETURN h.os AS os"],
    ));
    assert!(
        !table.contains('\u{202e}'),
        "table path leaked a raw override:\n{table}"
    );
    assert!(
        table.contains("\\u{202e}"),
        "table path did not escape the override:\n{table}"
    );

    // 2. Query JSON path (json_string): escaped to the JSON \u202e form.
    let qjson = stdout(&acetone(
        dir,
        &[
            "query",
            "--format",
            "json",
            "MATCH (h:Host) RETURN h.os AS os",
        ],
    ));
    assert!(
        !qjson.contains('\u{202e}'),
        "query --format json leaked a raw override:\n{qjson}"
    );
    assert!(
        qjson.contains("\\u202e"),
        "query --format json did not escape the override:\n{qjson}"
    );

    // 3. Read-command JSON path (emit_json residual pass): escaped, re-parses
    //    back to the original bytes.
    let gjson = stdout(&acetone(dir, &["get-node", "Host", "h1", "--json"]));
    assert!(
        !gjson.contains('\u{202e}'),
        "get-node --json leaked a raw override:\n{gjson}"
    );
    assert!(
        gjson.contains("\\u202e"),
        "get-node --json did not escape the override:\n{gjson}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&gjson)
        .unwrap_or_else(|e| panic!("get-node --json is not valid JSON ({e}):\n{gjson}"));
    // The escape round-trips: a JSON parser recovers the original character
    // from the \u202e escape we emitted.
    let recovered = parsed
        .get("properties")
        .and_then(|p| p.get("os"))
        .and_then(|v| v.as_str())
        .expect("os property in get-node --json");
    assert!(
        recovered.contains('\u{202e}'),
        "round-trip lost the original character: {recovered:?}"
    );
}

#[test]
fn override_in_a_branch_name_is_escaped_on_status_and_branch_list() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    repo_with_hostile_value(dir);
    // A branch needs a commit to point at.
    assert!(
        acetone(dir, &["commit", "-m", "seed"]).status.success(),
        "seed commit failed"
    );

    // Ref validation permits multibyte bidi, so a hostile clone can carry a
    // branch name that reorders following terminal text. Build it from an
    // escape (a raw U+202E in a string literal is denied by rustc's lint).
    let hostile_branch = format!("evil{}hidden", '\u{202e}');
    assert!(
        acetone(dir, &["branch", &hostile_branch]).status.success(),
        "creating the hostile branch failed"
    );
    assert!(
        acetone(dir, &["checkout", &hostile_branch])
            .status
            .success(),
        "checking out the hostile branch failed"
    );

    // `status` prints the current branch — must be escaped, never raw.
    let status = stdout(&acetone(dir, &["status"]));
    assert!(
        !status.contains('\u{202e}'),
        "status leaked a raw override in the branch name:\n{status}"
    );
    assert!(
        status.contains("\\u{202e}"),
        "status did not escape the branch-name override:\n{status}"
    );

    // `branch` (list) prints every branch name — same bar.
    let list = stdout(&acetone(dir, &["branch"]));
    assert!(
        !list.contains('\u{202e}'),
        "branch list leaked a raw override:\n{list}"
    );
    assert!(
        list.contains("\\u{202e}"),
        "branch list did not escape the override:\n{list}"
    );
}
