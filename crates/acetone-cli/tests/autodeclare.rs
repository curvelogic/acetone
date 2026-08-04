//! End-to-end relationship-type autodeclare through the shipped binary
//! (`acetone-nc91`, ADR-0060): the opt-in flag lets a CLI write coin a
//! predicate on demand, the coined type round-trips through `schema`, and
//! the default path still refuses — the criterion-3 CLI round-trip.

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

/// A repository with an `Entity` label and two nodes, built through the CLI.
fn seeded_repo(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());
    let declare = acetone(&repo, &["declare-label", "Entity", "--key", "id"]);
    assert!(declare.status.success(), "{}", stderr(&declare));
    for id in ["1", "2"] {
        let add = acetone(&repo, &["put-node", "Entity", id]);
        assert!(add.status.success(), "{}", stderr(&add));
    }
    repo
}

const COIN: &str = "MATCH (a:Entity {id: 1}), (b:Entity {id: 2}) \
                    CREATE (a)-[:MENTORS]->(b)";

#[test]
fn without_the_flag_an_unknown_type_refuses_with_guidance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let out = acetone(&repo, &["query", COIN]);
    assert!(!out.status.success(), "must refuse without --autodeclare");
    let err = stderr(&out);
    assert!(
        err.contains("unknown relationship type") && err.contains("declare-rel-type"),
        "guidance must name the declare command: {err}"
    );
}

#[test]
fn the_flag_coins_the_type_and_it_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);

    let out = acetone(&repo, &["query", "--autodeclare", COIN]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("autodeclared relationship type") && text.contains("MENTORS"),
        "the coinage must be announced: {text}"
    );

    // Round-trip: `schema` lists the coined type.
    let schema = acetone(&repo, &["schema"]);
    assert!(schema.status.success(), "{}", stderr(&schema));
    assert!(
        stdout(&schema).contains("MENTORS"),
        "schema must list the coined type: {}",
        stdout(&schema)
    );

    // And a plain query — no flag — matches through it.
    let read = acetone(
        &repo,
        &[
            "query",
            "MATCH (:Entity {id: 1})-[:MENTORS]->(b) RETURN b.id",
        ],
    );
    assert!(read.status.success(), "{}", stderr(&read));
    assert!(stdout(&read).contains('2'), "{}", stdout(&read));
}

#[test]
fn the_flag_never_coins_on_a_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let out = acetone(
        &repo,
        &["query", "--autodeclare", "MATCH ()-[r:NEVER]->() RETURN r"],
    );
    assert!(!out.status.success(), "a read must not coin");
    assert!(stderr(&out).contains("unknown relationship type"));
    let schema = acetone(&repo, &["schema"]);
    assert!(
        !stdout(&schema).contains("NEVER"),
        "no schema mutation on a read: {}",
        stdout(&schema)
    );
}

#[test]
fn the_shell_meta_command_toggles_coinage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seeded_repo(&dir);
    let bin = env!("CARGO_BIN_EXE_acetone");
    let script = format!(
        ":autodeclare on\n{COIN};\n:autodeclare off\nMATCH (a:Entity {{id: 2}}), (b:Entity {{id: 1}}) CREATE (a)-[:UNSANCTIONED]->(b);\n"
    );
    let mut child = Command::new(bin)
        .args(["--repo", repo.to_str().unwrap(), "shell"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn shell");
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(script.as_bytes())
        .expect("write script");
    let out = child.wait_with_output().expect("shell run");
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        text.contains("autodeclared relationship type") && text.contains("MENTORS"),
        "shell coinage must announce: {text}"
    );
    assert!(
        text.contains("unknown relationship type"),
        "after :autodeclare off the coinage must refuse again: {text}"
    );
    let schema = acetone(&repo, &["schema"]);
    assert!(stdout(&schema).contains("MENTORS"));
    assert!(!stdout(&schema).contains("UNSANCTIONED"));
}
