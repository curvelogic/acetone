//! End-to-end `acetone declare-index` / `reindex` tests over the real binary.

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

#[test]
fn declare_index_reindex_and_fsck() {
    let dir = tempfile::tempdir().expect("tmp");
    let repo = dir.path().join("repo");
    assert!(init(&repo).status.success());

    // Schema + a couple of nodes.
    assert!(
        acetone(&repo, &["declare-label", "Host", "--key", "name"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "schema"]).status.success());
    assert!(
        acetone(&repo, &["put-node", "Host", "web1", "--prop", "region=eu"])
            .status
            .success()
    );
    assert!(
        acetone(&repo, &["put-node", "Host", "db1", "--prop", "region=us"])
            .status
            .success()
    );
    assert!(acetone(&repo, &["commit", "-m", "hosts"]).status.success());

    // Declare an index over existing nodes.
    let out = acetone(
        &repo,
        &[
            "declare-index",
            "host_region",
            "--label",
            "Host",
            "--property",
            "region",
        ],
    );
    assert!(out.status.success(), "declare-index: {}", stderr(&out));
    assert!(
        stdout(&out).contains("declared index"),
        "got: {}",
        stdout(&out)
    );
    assert!(acetone(&repo, &["commit", "-m", "index"]).status.success());

    // fsck clean (index consistent with nodes).
    let out = acetone(&repo, &["fsck"]);
    assert!(out.status.success(), "fsck: {}", stderr(&out));

    // reindex is a no-op on an already-consistent repo, and succeeds.
    let out = acetone(&repo, &["reindex"]);
    assert!(out.status.success(), "reindex: {}", stderr(&out));
    assert!(stdout(&out).contains("reindexed"), "got: {}", stdout(&out));

    // Still clean afterwards.
    assert!(acetone(&repo, &["fsck"]).status.success());
}
