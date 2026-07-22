//! End-to-end CLI tests for `acetone init --co-tenant <graph>` (acetone-xg6):
//! the graph is created inside an existing git repository, on its own ref
//! namespace, alongside the code — and is then usable through the shipped CLI.
//! Also covers the init preconditions (acetone-eo7 edge cases).

use std::path::Path;
use std::process::{Command, Output};

fn acetone(repo: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full = vec!["--repo", repo.to_str().unwrap()];
    full.extend_from_slice(args);
    Command::new(bin).args(&full).output().expect("run acetone")
}

fn git(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Code Dev",
            "-c",
            "user.email=dev@example.invalid",
        ])
        .args(args)
        .output()
        .expect("run git")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("utf8")
}
fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("utf8")
}

/// A git repository with one code commit on `main`.
fn code_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("project");
    std::fs::create_dir(&repo).expect("mkdir");
    assert!(
        git(&repo, &["-c", "init.defaultBranch=main", "init"])
            .status
            .success()
    );
    std::fs::write(repo.join("code.txt"), "source").expect("write");
    assert!(git(&repo, &["add", "code.txt"]).status.success());
    assert!(
        git(&repo, &["commit", "-m", "code: initial"])
            .status
            .success()
    );
    (dir, repo)
}

#[test]
fn init_co_tenant_creates_the_graph_beside_code_and_is_cli_usable() {
    let (_dir, repo) = code_repo();
    let head_before = stdout(&git(&repo, &["symbolic-ref", "HEAD"]));
    let main_before = stdout(&git(&repo, &["rev-parse", "refs/heads/main"]));

    let out = acetone(&repo, &["init", "--co-tenant", "assets"]);
    assert!(
        out.status.success(),
        "init --co-tenant failed: {}",
        stderr(&out)
    );
    let msg = stdout(&out);
    assert!(
        msg.contains("co-tenant") && msg.contains("assets"),
        "unexpected init message: {msg}"
    );

    // The graph's marker exists; the code refs and git HEAD are untouched.
    let refs = stdout(&git(&repo, &["for-each-ref", "--format=%(refname)"]));
    assert!(
        refs.contains("refs/acetone/graphs/assets"),
        "co-tenant marker missing; refs:\n{refs}"
    );
    assert_eq!(
        stdout(&git(&repo, &["symbolic-ref", "HEAD"])),
        head_before,
        "git HEAD must be untouched"
    );
    assert_eq!(
        stdout(&git(&repo, &["rev-parse", "refs/heads/main"])),
        main_before,
        "the code branch must be untouched"
    );

    // The shipped CLI opens the co-tenant graph.
    let status = acetone(&repo, &["status"]);
    assert!(
        status.status.success(),
        "status failed: {}",
        stderr(&status)
    );
    assert!(
        stdout(&status).contains("On branch main"),
        "status did not open the co-tenant graph: {}",
        stdout(&status)
    );
}

#[test]
fn init_co_tenant_rejects_a_bad_graph_name() {
    let (_dir, repo) = code_repo();
    let out = acetone(&repo, &["init", "--co-tenant", "a/b"]);
    assert!(
        !out.status.success(),
        "a graph name with a slash must be rejected"
    );
}

#[test]
fn init_co_tenant_on_a_non_git_directory_errors_clearly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = dir.path().join("not-a-repo");
    std::fs::create_dir(&plain).expect("mkdir");
    let out = acetone(&plain, &["init", "--co-tenant", "assets"]);
    assert!(
        !out.status.success(),
        "co-tenant init needs an existing git repository"
    );
}

#[test]
fn init_co_tenant_refuses_a_second_graph_and_names_the_existing_one() {
    let (_dir, repo) = code_repo();
    assert!(
        acetone(&repo, &["init", "--co-tenant", "assets"])
            .status
            .success()
    );
    let out = acetone(&repo, &["init", "--co-tenant", "other"]);
    assert!(!out.status.success(), "a second graph must be refused");
    assert!(
        stderr(&out).contains("assets") || stdout(&out).contains("assets"),
        "the error should name the existing graph: {}",
        stderr(&out)
    );
}

#[test]
fn init_co_tenant_rejects_a_legacy_standalone_workspace() {
    // A pre-ADR-0014 standalone acetone repository has the legacy shared
    // workspace ref (`refs/acetone/workspaces/default`) but not the modern
    // per-worktree one. Co-tenant init must refuse it too (acetone-eo7).
    let (_dir, repo) = code_repo();
    // Fabricate the legacy workspace ref pointing at a real object. An empty
    // tree is a well-known object git can write without stdin plumbing.
    let empty_tree = stdout(&git(
        &repo,
        &["hash-object", "-w", "-t", "tree", "/dev/null"],
    ));
    let empty_tree = empty_tree.trim();
    assert!(
        git(
            &repo,
            &["update-ref", "refs/acetone/workspaces/default", empty_tree]
        )
        .status
        .success(),
        "failed to fabricate the legacy workspace ref"
    );
    let out = acetone(&repo, &["init", "--co-tenant", "assets"]);
    assert!(
        !out.status.success(),
        "co-tenant init must reject a repo carrying a legacy standalone workspace"
    );
}
