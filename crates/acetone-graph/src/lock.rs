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
                    .map(|s| sanitise_holder(s.trim()))
                    .unwrap_or_else(|_| "unknown holder".to_owned());
                Err(GraphError::Locked { holder, path })
            }
            Err(cause) => Err(GraphError::LockIo { path, cause }),
        }
    }

    /// The lock file's path (for diagnostics and tests).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Sanitise the lock file's holder string before it enters
/// [`GraphError::Locked`]'s message (acetone-6tt, defence in depth): the file
/// is read verbatim from the local git dir, so a poisoned lock could carry
/// raw ANSI/control bytes or bidi overrides into the terminal. Escapes every
/// control character plus the bidirectional formatting set (U+061C,
/// U+200E–200F, U+202A–202E, U+2066–2069 — the class the CLI escapes on all
/// repository-controlled output), and caps the result so a multi-KB file
/// cannot balloon the error message. Well-formed acetone lock contents
/// (`pid=… unix-time=…`) pass through untouched.
fn sanitise_holder(raw: &str) -> String {
    const MAX_CHARS: usize = 200;
    let is_unsafe = |c: char| {
        c.is_control()
            || matches!(c,
                '\u{061C}'                 // ARABIC LETTER MARK
                | '\u{200E}' | '\u{200F}'  // LRM, RLM
                | '\u{202A}'..='\u{202E}'  // LRE, RLE, PDF, LRO, RLO
                | '\u{2066}'..='\u{2069}'  // LRI, RLI, FSI, PDI
            )
    };
    let mut out = String::new();
    for (index, c) in raw.chars().enumerate() {
        if index == MAX_CHARS {
            out.push('…');
            break;
        }
        if is_unsafe(c) {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
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
    fn poisoned_lock_holder_is_sanitised_in_the_error() {
        // acetone-6tt: the holder string is read verbatim from a file a local
        // attacker could poison; ANSI/control bytes and bidi overrides must
        // never reach the terminal raw through the error message.
        let dir = tempfile::tempdir().expect("tempdir");
        let hostile = "pid=1 \x1b[31mred\u{202e}desrever";
        std::fs::write(dir.path().join(WRITER_LOCK_FILE), hostile).expect("plant lock");
        let err = WriteLock::acquire(dir.path()).expect_err("must refuse");
        match &err {
            GraphError::Locked { holder, .. } => {
                assert!(!holder.contains('\x1b'), "raw ESC leaked: {holder:?}");
                assert!(
                    !holder.contains('\u{202e}'),
                    "raw bidi override leaked: {holder:?}"
                );
                assert!(
                    holder.contains("\\u{1b}") && holder.contains("\\u{202e}"),
                    "escaped forms expected, got: {holder}"
                );
                assert!(holder.contains("pid=1"), "printable text kept: {holder}");
            }
            other => panic!("expected Locked, got {other:?}"),
        }
        // What the user actually sees (Display) is clean too.
        let message = err.to_string();
        assert!(!message.contains('\x1b') && !message.contains('\u{202e}'));
    }

    #[test]
    fn oversize_lock_holder_is_truncated() {
        // A poisoned multi-KB lock file must not balloon the error message.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(WRITER_LOCK_FILE), "x".repeat(10_000)).expect("plant lock");
        let message = WriteLock::acquire(dir.path())
            .expect_err("must refuse")
            .to_string();
        assert!(
            message.len() < 1_000,
            "holder must be capped, message is {} bytes",
            message.len()
        );
        assert!(message.contains('…'), "truncation must be visible");
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
