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
//! **No automatic stale-lock breaking for CLI writers** (ADR-0010): if the
//! holding process died, the next CLI writer gets a typed
//! [`GraphError::Locked`] naming the pid and the file to delete once no
//! acetone process is running. Readers never touch this lock (MVCC —
//! they are pinned to immutable roots).
//!
//! **Daemon-only stale-lock recovery** (ADR-0074 §8, acetone-pz0k.6): a
//! long-lived `acetone serve` would otherwise crash-loop on a lock left by
//! a SIGKILLed writer. [`break_stale_lock`] lets the daemon — and ONLY the
//! daemon (the CLI path above is unchanged) — break a lock whose recorded
//! pid names no live process. It errs hard on the side of *not* breaking a
//! live lock: a pid that still names *any* running process is treated as
//! live and refused (the pid-reuse refinement — checking the holder's
//! identity and start time when the pid is reused — is a follow-up). The
//! break is `unlink`-then-`O_EXCL`-recreate on re-acquire, and the daemon
//! serialises all recovery behind one mutex, so two recoverers cannot race.

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
            Err(source) => Err(GraphError::LockIo { path, source }),
        }
    }

    /// The lock file's path (for diagnostics and tests).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The result of a stale-lock recovery attempt (acetone-pz0k.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleLockOutcome {
    /// The lock was stale (its pid names no live process) and was removed;
    /// the caller should retry acquiring.
    Broken,
    /// The lock is (or may be) held by a live process — left untouched.
    Live,
    /// No lock file was present (already released) — the caller should just
    /// retry acquiring.
    Absent,
}

/// Break the worktree's writer lock **iff** it is stale — its recorded pid
/// names no live process (ADR-0074 §8). For the DAEMON write path only; the
/// CLI never calls this. Returns [`StaleLockOutcome`]. The caller MUST
/// serialise concurrent recovery (the daemon holds one mutex) so two
/// recoverers cannot both remove-and-recreate; `O_EXCL` on the caller's
/// re-acquire is the final backstop.
///
/// **Conservative by design**: a pid that still names any running process is
/// treated as `Live` and the lock is left in place — even though the pid
/// *could* have been reused by an unrelated process. That errs on the side
/// of never breaking a live lock (a wrong break is a double-writer, the very
/// thing the lock prevents); the cost is that the rare pid-reuse case still
/// needs manual recovery, exactly as today (the pid-reuse refinement using
/// the holder's identity and start time is a tracked follow-up).
pub fn break_stale_lock(git_dir: &Path) -> Result<StaleLockOutcome, GraphError> {
    let path = git_dir.join(WRITER_LOCK_FILE);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        // Gone already (a writer released between the Locked error and here).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StaleLockOutcome::Absent);
        }
        Err(source) => return Err(GraphError::LockIo { path, source }),
    };
    // A lock whose holder we cannot parse is left ALONE (conservative): we
    // only ever break a lock we can positively judge stale.
    let Some(pid) = parse_lock_pid(&contents) else {
        return Ok(StaleLockOutcome::Live);
    };
    if pid_is_live(pid) {
        return Ok(StaleLockOutcome::Live);
    }
    // Stale: remove it. A NotFound here means another (serialised) recoverer
    // already removed it — treat as broken; the caller re-acquires under
    // O_EXCL regardless.
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(StaleLockOutcome::Broken),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StaleLockOutcome::Broken),
        Err(source) => Err(GraphError::LockIo { path, source }),
    }
}

/// Parse the pid from a lock file's `pid=<n> unix-time=<t>` contents as a
/// **positive `i32`** — a real POSIX pid. `None` if the shape is not exactly
/// that (an unparseable lock is never broken). Parsing directly as `i32`
/// (not `u32`-then-cast) rejects a corrupted/adversarial pid above
/// `i32::MAX`, which would otherwise cast to a negative value and make
/// `kill` probe a process *group* rather than a process (a wrong liveness
/// answer in a corruption-critical path).
fn parse_lock_pid(contents: &str) -> Option<i32> {
    contents
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("pid="))
        .and_then(|n| n.parse::<i32>().ok())
        .filter(|&pid| pid > 0)
}

/// Whether a process with `pid` currently exists, via `kill(pid, 0)` (POSIX,
/// both release targets — ADR-0074 §8), through nix's SAFE wrapper so the
/// crate keeps `#![forbid(unsafe_code)]`. `pid` is a validated positive
/// `i32` (a real process, never a `kill` process-group sentinel). `Ok` means
/// the process exists; `ESRCH` means it does not; any other error (e.g.
/// `EPERM` — it exists but is another user's) errs toward "live" (do not
/// break).
fn pid_is_live(pid: i32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
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

    /// Plant a lock file with the given contents in `git_dir`.
    fn plant_lock(git_dir: &Path, contents: &str) {
        std::fs::write(git_dir.join(WRITER_LOCK_FILE), contents).expect("plant lock");
    }

    /// A pid that names no live process: spawn a child, wait for it (so it is
    /// reaped, not a zombie), and return its now-dead pid. Racy only if the OS
    /// reuses the pid before the test reads it — vanishingly unlikely in-test.
    fn a_dead_pid() -> u32 {
        let child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let mut child = child;
        child.wait().expect("reap");
        pid
    }

    #[test]
    fn a_lock_naming_a_dead_pid_is_broken() {
        let dir = tempfile::tempdir().expect("tempdir");
        plant_lock(dir.path(), &format!("pid={} unix-time=1\n", a_dead_pid()));
        assert_eq!(
            break_stale_lock(dir.path()).expect("break"),
            StaleLockOutcome::Broken
        );
        // Broken means removed — a fresh acquire now succeeds.
        assert!(!dir.path().join(WRITER_LOCK_FILE).exists());
        WriteLock::acquire(dir.path()).expect("acquire after break");
    }

    #[test]
    fn a_lock_naming_a_live_pid_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        // This very test process is unmistakably live.
        plant_lock(
            dir.path(),
            &format!("pid={} unix-time=1\n", std::process::id()),
        );
        assert_eq!(
            break_stale_lock(dir.path()).expect("break"),
            StaleLockOutcome::Live
        );
        assert!(dir.path().join(WRITER_LOCK_FILE).exists(), "live lock kept");
    }

    #[test]
    fn an_absent_lock_reports_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            break_stale_lock(dir.path()).expect("break"),
            StaleLockOutcome::Absent
        );
    }

    #[test]
    fn an_unparseable_lock_is_never_broken() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No `pid=` — we cannot judge it stale, so we must not break it.
        plant_lock(dir.path(), "garbage from some other tool\n");
        assert_eq!(
            break_stale_lock(dir.path()).expect("break"),
            StaleLockOutcome::Live
        );
        assert!(dir.path().join(WRITER_LOCK_FILE).exists());
        // pid=0 is also refused (never a real acquirable pid).
        plant_lock(dir.path(), "pid=0 unix-time=1\n");
        assert_eq!(
            break_stale_lock(dir.path()).expect("break"),
            StaleLockOutcome::Live
        );
        // A pid above i32::MAX (corrupt/adversarial) must NOT be broken: it
        // would cast to a negative kill() process-group sentinel. Parsed as a
        // positive i32, it is unparseable -> Live.
        plant_lock(dir.path(), "pid=4000000000 unix-time=1\n");
        assert_eq!(
            break_stale_lock(dir.path()).expect("break"),
            StaleLockOutcome::Live
        );
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
