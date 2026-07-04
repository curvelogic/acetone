//! Shared helpers for the acetone-store integration tests.
//!
//! Each integration test binary compiles this module separately and uses a
//! different subset of it, so unused-by-one-binary is expected.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use acetone_store::GitStore;

/// A fresh bare-repository store in a temp dir.
pub fn new_store() -> (tempfile::TempDir, GitStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = GitStore::create(&dir.path().join("repo.git")).expect("create store");
    (dir, store)
}

/// Options with a non-default object size cap.
pub fn capped_options(max_chunk_size: u64) -> acetone_store::GitStoreOptions {
    let mut options = acetone_store::GitStoreOptions::default();
    options.max_chunk_size = max_chunk_size;
    options
}

/// A fresh bare-repository store with a non-default object size cap.
pub fn new_capped_store(max_chunk_size: u64) -> (tempfile::TempDir, GitStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = GitStore::create_with(&dir.path().join("repo.git"), capped_options(max_chunk_size))
        .expect("create store");
    (dir, store)
}

/// Path of the repository created by [`new_store`].
pub fn repo_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("repo.git")
}

/// Run a git command in `repo`, panicking (with full output) on failure.
/// Tests MAY shell out to git to verify interop; library code never does.
pub fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run a git command in `repo` with `stdin`, panicking on failure.
pub fn git_stdin(repo: &Path, args: &[&str], stdin: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(stdin)
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait for git");
    assert!(
        out.status.success(),
        "git {args:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}
