//! Repository auto-discovery from a subdirectory (ADR-0034, bead
//! acetone-7bn.12): `acetone <cmd>` works from any subdirectory of a
//! repository, walking up to find it — like `git -C`. Init is exempt: it
//! creates a repository at the exact path, never reusing an enclosing one.

use std::path::Path;
use std::process::{Command, Output};

/// Run `acetone --repo <repo> <args...>` with the process cwd unchanged.
fn acetone(repo: &Path, args: &[&str]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut full_args = vec!["--repo", repo.to_str().unwrap()];
    full_args.extend_from_slice(args);
    Command::new(bin)
        .args(&full_args)
        .output()
        .expect("failed to run acetone binary")
}

/// Run `acetone <args...>` with a given process cwd and environment, no
/// `--repo` (so it defaults to `.`). `envs` are extra environment variables.
fn acetone_in(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let bin = env!("CARGO_BIN_EXE_acetone");
    let mut cmd = Command::new(bin);
    cmd.current_dir(cwd).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run acetone binary")
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

/// `--repo <subdir>` of an initialised repository discovers the enclosing
/// root (the acetone repo is bare, so a subdirectory lives inside it).
#[test]
fn repo_flag_subdirectory_discovers_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo.git");
    assert!(init(&repo).status.success());

    let sub = repo.join("some/deep/dir");
    std::fs::create_dir_all(&sub).expect("mkdir subdir");

    let out = acetone(&sub, &["status"]);
    assert!(
        out.status.success(),
        "status from a subdirectory should discover the root: {}",
        stderr(&out)
    );
    assert!(stdout(&out).contains("On branch main"));
}

/// The same, but through the process cwd and the default `--repo .`.
#[test]
fn cwd_subdirectory_with_default_repo_discovers_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo.git");
    assert!(init(&repo).status.success());

    let sub = repo.join("nested/work");
    std::fs::create_dir_all(&sub).expect("mkdir subdir");

    // No --repo: defaults to `.`, i.e. the process cwd (the subdirectory).
    let out = acetone_in(&sub, &["status"], &[]);
    assert!(
        out.status.success(),
        "status with cwd in a subdirectory and --repo . should discover the root: {}",
        stderr(&out)
    );
    assert!(stdout(&out).contains("On branch main"));
}

/// An explicit `--repo <root>` still opens immediately (regression: the
/// common, exact case must keep working).
#[test]
fn explicit_repo_root_opens_immediately() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo.git");
    assert!(init(&repo).status.success());

    let out = acetone(&repo, &["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("On branch main"));
}

/// A directory with no repository ancestor fails with the clear
/// not-found error (and does not panic).
#[test]
fn no_repository_ancestor_fails_clearly() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A bare temp directory with no git repository above it (temp dirs are
    // not created inside a repository).
    let out = acetone(dir.path(), &["status"]);
    assert!(
        !out.status.success(),
        "status with no repository ancestor should fail"
    );
    let err = stderr(&out);
    assert!(
        err.contains("no acetone repository") && err.contains("acetone init"),
        "error should name the problem and point at `acetone init`, got: {err}"
    );
    // Cheap panic check: a Rust panic prints this to stderr.
    assert!(
        !err.contains("panicked"),
        "must fail cleanly, not panic: {err}"
    );
}

/// `acetone init <path>` creates a repository at exactly `<path>` even when
/// `<path>` is inside an existing repository — init never discovers or
/// reuses an enclosing repository (the isolation-of-init guarantee).
#[test]
fn init_does_not_discover_enclosing_repository() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outer = dir.path().join("outer.git");
    assert!(init(&outer).status.success());

    // A nested path inside the outer repository's directory tree. (git/gix
    // init creates the repo dir itself but not missing parents.)
    std::fs::create_dir_all(outer.join("nested")).expect("mkdir nested");
    let inner = outer.join("nested/inner.git");
    let out = init(&inner);
    assert!(
        out.status.success(),
        "init inside an existing repo should still create at the exact path: {}",
        stderr(&out)
    );
    assert!(stdout(&out).contains("Initialized empty acetone repository"));

    // The inner repository is a real, distinct repository: git recognises it
    // as a git dir in its own right.
    assert!(
        inner.join("HEAD").is_file(),
        "init should have created a git dir at the exact inner path"
    );

    // Writing to the inner repository from its own path is independent of the
    // outer one. Put a node in inner and confirm outer never sees it.
    let out = acetone(&inner, &["put-node", "Marker", "inner"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = acetone(&inner, &["status"]);
    assert!(stdout(&out).contains("nodes: 1"), "{}", stdout(&out));

    let out = acetone(&outer, &["status"]);
    assert!(
        stdout(&out).contains("nodes: 0"),
        "outer repository must not see inner's write: {}",
        stdout(&out)
    );
}

/// `GIT_CEILING_DIRECTORIES` bounds the walk: with the ceiling set to the
/// subdirectory's parent, discovery stops before reaching the repository
/// root and fails, even though a repository does enclose the start path.
#[test]
fn ceiling_directories_bounds_the_walk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo.git");
    assert!(init(&repo).status.success());

    let sub = repo.join("a/b");
    std::fs::create_dir_all(&sub).expect("mkdir subdir");

    // Ceiling at `repo.git/a`: the walk from `repo.git/a/b` may not climb to
    // or past it, so it never reaches `repo.git`. GIT_CEILING_DIRECTORIES
    // entries must be absolute.
    let ceiling = repo.join("a");
    let out = acetone_in(
        &sub,
        &["status"],
        &[("GIT_CEILING_DIRECTORIES", ceiling.to_str().unwrap())],
    );
    assert!(
        !out.status.success(),
        "discovery should stop at the ceiling and not find the repo: {}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("no acetone repository"),
        "expected the not-found error, got: {}",
        stderr(&out)
    );

    // Sanity: without the ceiling, the same start path discovers the root.
    let out = acetone_in(&sub, &["status"], &[]);
    assert!(
        out.status.success(),
        "without a ceiling the repo is found: {}",
        stderr(&out)
    );
}
