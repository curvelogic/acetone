//! The single-writer lock (spec §4, ADR-0010, ADR-0014).
//!
//! One writer per **worktree**, enforced by a lock file in the worktree's
//! own git directory: `acetone-writer.lock`, created with
//! `O_CREAT | O_EXCL` (atomic on every filesystem git itself supports) and
//! holding the owner's pid and acquisition time. Placing it in the
//! per-worktree git dir (ADR-0014) rather than the shared common dir makes
//! writers in different worktrees independent — matching git's
//! per-worktree `index.lock`. The lock is held for the life of a
//! [`WriteLock`] — a whole write transaction — unlike the store layer's
//! `acetone-refs.lock`, which stays common and guards single ref updates
//! for milliseconds.
//!
//! **No automatic stale-lock breaking in v0.1** (ADR-0010): if the
//! holding process died, the next writer gets a typed
//! [`GraphError::Locked`] naming the pid and the file to delete once no
//! acetone process is running. Readers never touch this lock (MVCC —
//! they are pinned to immutable roots).

use crate::error::GraphError;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// File name of the single-writer lock within the per-worktree git dir.
pub const WRITER_LOCK_FILE: &str = "acetone-writer.lock";

/// Held for the duration of one write transaction; releases (removes the
/// lock file) on drop.
#[derive(Debug)]
pub struct WriteLock {
    path: PathBuf,
}

impl WriteLock {
    /// Acquire the worktree's single-writer lock (in `git_dir`, the
    /// per-worktree git directory), or fail with [`GraphError::Locked`]
    /// describing the current holder.
    pub fn acquire(git_dir: &Path) -> Result<WriteLock, GraphError> {
        let path = git_dir.join(WRITER_LOCK_FILE);
        let mut open_options = std::fs::OpenOptions::new();
        open_options.write(true).create_new(true);
        match open_options.open(&path) {
            Ok(mut file) => {
                let unix_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                // Best-effort holder info; the lock exists regardless.
                let _ = writeln!(file, "pid={} unix-time={}", std::process::id(), unix_secs);
                Ok(WriteLock { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path)
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_else(|_| "unknown holder".to_owned());
                Err(GraphError::Locked { holder, path })
            }
            Err(source) => Err(GraphError::LockIo { path, source }),
        }
    }

    /// The lock file's path (for diagnostics and tests).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        // Failure to remove leaves a stale lock, reported with recovery
        // instructions on the next acquire; nothing useful to do here.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = WriteLock::acquire(dir.path()).expect("first acquire");
        let second = WriteLock::acquire(dir.path());
        match second {
            Err(GraphError::Locked { holder, path }) => {
                assert!(holder.contains("pid="), "holder info recorded: {holder}");
                assert_eq!(path, dir.path().join(WRITER_LOCK_FILE));
            }
            other => panic!("expected Locked, got {other:?}"),
        }
        drop(lock);
        let third = WriteLock::acquire(dir.path()).expect("acquire after release");
        assert!(third.path().exists());
    }

    #[test]
    fn stale_lock_reports_manual_recovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(WRITER_LOCK_FILE), "pid=999999 unix-time=0")
            .expect("plant stale lock");
        let err = WriteLock::acquire(dir.path()).expect_err("must refuse");
        let message = err.to_string();
        assert!(
            message.contains("remove") && message.contains(WRITER_LOCK_FILE),
            "error must carry recovery instructions: {message}"
        );
    }
}
