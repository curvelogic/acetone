//! Zero-width spoofing hardening (bead acetone-0ds, the half left open by
//! acetone-7bn.19): invisible format characters (ZWSP, ZWNJ, ZWJ, word
//! joiner, BOM, soft hyphen) in **identifier-shaped** repository-controlled
//! text — labels, property keys, relationship types, branch names — are
//! escaped before the terminal, so `Host` and `Ho<ZWSP>st` cannot render
//! identically. **Value** output is exempt: emoji ZWJ sequences are
//! legitimate property data and must render untouched.

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

#[test]
fn zero_width_in_identifiers_is_escaped_but_values_keep_emoji() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    Command::new(bin())
        .args(["init", dir.to_str().unwrap()])
        .output()
        .expect("init");
    assert!(
        acetone(dir, &["declare-label", "Team", "--key", "id"])
            .status
            .success()
    );
    // A property *key* hiding a ZWSP, and a property *value* holding a
    // legitimate emoji ZWJ sequence (U+200D between the two emoji).
    let hostile_key_prop = format!("na{}me=x", '\u{200b}');
    assert!(
        acetone(
            dir,
            &[
                "put-node",
                "Team",
                "1",
                "--prop",
                &hostile_key_prop,
                "--prop",
                "family=👩‍👧",
            ],
        )
        .status
        .success()
    );

    let table = stdout(&acetone(dir, &["query", "MATCH (t:Team) RETURN t"]));
    // The identifier-shaped property key is escaped, never invisible.
    assert!(
        !table.contains('\u{200b}'),
        "table output leaked a raw ZWSP in a property key:\n{table}"
    );
    assert!(
        table.contains("\\u{200b}"),
        "table output did not escape the ZWSP:\n{table}"
    );
    // The value keeps its emoji ZWJ sequence byte-for-byte (U+200D intact).
    assert!(
        table.contains("👩\u{200d}👧"),
        "value output must keep the emoji ZWJ sequence untouched:\n{table}"
    );
}

#[test]
fn zero_width_in_a_label_is_escaped_on_query_output() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    Command::new(bin())
        .args(["init", dir.to_str().unwrap()])
        .output()
        .expect("init");
    let hostile_label = format!("Te{}am", '\u{200b}');
    assert!(
        acetone(dir, &["declare-label", &hostile_label, "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(dir, &["put-node", &hostile_label, "1"])
            .status
            .success()
    );
    let table = stdout(&acetone(dir, &["query", "MATCH (n) RETURN n"]));
    assert!(
        !table.contains('\u{200b}'),
        "query output leaked a raw ZWSP in a label:\n{table}"
    );
    assert!(
        table.contains("\\u{200b}"),
        "query output did not escape the label's ZWSP:\n{table}"
    );
}

/// The commit hash from `acetone commit` output ("committed <hex>").
fn commit_hex(output: &Output) -> String {
    let text = String::from_utf8(output.stdout.clone()).expect("stdout is not UTF-8");
    text.split_whitespace()
        .last()
        .expect("commit output has a hash")
        .to_string()
}

#[test]
fn projected_identifier_columns_escape_zero_width_in_table_and_csv_not_json() {
    // PR #171 review finding 1: identifiers projected as plain String cells —
    // `labels(n)`, `keys(n)`, `type(r)`, `CALL acetone.diff ... YIELD label` —
    // must meet the same identifier bar as `RETURN n`, in table AND csv;
    // `--format json` deliberately keeps the raw character (lossless
    // round-trip for consumers).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    Command::new(bin())
        .args(["init", dir.to_str().unwrap()])
        .output()
        .expect("init");
    let hostile_label = format!("Te{}am", '\u{200b}');
    let hostile_prop = format!("na{}me=x", '\u{200b}');
    let hostile_rtype = format!("LI{}NKS", '\u{200b}');
    assert!(
        acetone(dir, &["declare-label", &hostile_label, "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(
            dir,
            &["put-node", &hostile_label, "1", "--prop", &hostile_prop]
        )
        .status
        .success()
    );
    let c1 = commit_hex(&acetone(dir, &["commit", "-m", "one"]));
    assert!(
        acetone(dir, &["put-node", &hostile_label, "2"])
            .status
            .success()
    );
    let c2 = commit_hex(&acetone(dir, &["commit", "-m", "two"]));
    assert!(
        acetone(
            dir,
            &[
                "put-edge",
                &hostile_label,
                "1",
                &hostile_rtype,
                &hostile_label,
                "2",
            ],
        )
        .status
        .success()
    );

    let escaped = |out: &str, probe: &str| {
        assert!(
            !out.contains('\u{200b}'),
            "{probe} leaked a raw ZWSP:\n{out}"
        );
        assert!(
            out.contains("\\u{200b}"),
            "{probe} did not escape the ZWSP:\n{out}"
        );
    };

    // labels(n): escaped in table and csv…
    let labels_query = "MATCH (n) RETURN labels(n) AS l";
    escaped(
        &stdout(&acetone(dir, &["query", labels_query])),
        "labels/table",
    );
    escaped(
        &stdout(&acetone(dir, &["query", labels_query, "--format", "csv"])),
        "labels/csv",
    );
    // …but raw (round-trippable) in JSON.
    let json = stdout(&acetone(dir, &["query", labels_query, "--format", "json"]));
    assert!(
        json.contains('\u{200b}'),
        "json must keep the raw character for round-trip:\n{json}"
    );
    assert!(
        !json.contains("\\u{200b}"),
        "json must not carry terminal escapes:\n{json}"
    );

    // keys(n).
    let keys_query = "MATCH (n) RETURN keys(n) AS k";
    escaped(&stdout(&acetone(dir, &["query", keys_query])), "keys/table");
    escaped(
        &stdout(&acetone(dir, &["query", keys_query, "--format", "csv"])),
        "keys/csv",
    );

    // type(r).
    let type_query = "MATCH (a)-[r]->(b) RETURN type(r) AS t";
    escaped(&stdout(&acetone(dir, &["query", type_query])), "type/table");
    escaped(
        &stdout(&acetone(dir, &["query", type_query, "--format", "csv"])),
        "type/csv",
    );

    // CALL acetone.diff YIELD label.
    let diff_query = format!("CALL acetone.diff('{c1}', '{c2}') YIELD label RETURN label");
    escaped(
        &stdout(&acetone(dir, &["query", &diff_query])),
        "diff/table",
    );
    escaped(
        &stdout(&acetone(dir, &["query", &diff_query, "--format", "csv"])),
        "diff/csv",
    );
}

#[test]
fn zero_width_in_a_branch_name_is_escaped_on_status_and_branch_list() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    Command::new(bin())
        .args(["init", dir.to_str().unwrap()])
        .output()
        .expect("init");
    assert!(
        acetone(dir, &["declare-label", "N", "--key", "id"])
            .status
            .success()
    );
    assert!(
        acetone(dir, &["commit", "-m", "seed"]).status.success(),
        "seed commit failed"
    );

    // Ref validation permits multibyte invisibles, so a hostile clone can
    // carry a branch whose name spoofs another's.
    let hostile_branch = format!("ma{}in", '\u{200b}');
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

    let status = stdout(&acetone(dir, &["status"]));
    assert!(
        !status.contains('\u{200b}'),
        "status leaked a raw ZWSP in the branch name:\n{status}"
    );
    assert!(
        status.contains("\\u{200b}"),
        "status did not escape the branch-name ZWSP:\n{status}"
    );

    let list = stdout(&acetone(dir, &["branch"]));
    assert!(
        !list.contains('\u{200b}'),
        "branch list leaked a raw ZWSP:\n{list}"
    );
    assert!(
        list.contains("\\u{200b}"),
        "branch list did not escape the ZWSP:\n{list}"
    );
}
