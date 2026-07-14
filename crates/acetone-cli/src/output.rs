//! Panic-free stdout for a CLI that lives in pipelines.
//!
//! `println!` panics when the consumer closes the pipe early
//! (`acetone log | grep -q ...`, `| head`). For a CLI that is normal
//! termination, not an error: exit 0 quietly. Exiting 0 rather than
//! re-raising SIGPIPE keeps `pipefail` scripts working and avoids an
//! `unsafe` libc dependency. Any other stdout failure is real and exits 1.
//!
//! Caveat for status-bearing commands (e.g. `fsck`): a consumer that
//! closes the pipe early converts the command's failure verdict into
//! exit 0. Scripts must grep for the verdict line, not rely on the exit
//! code through a truncating pipe.

/// `println!` that treats a closed stdout as clean process exit.
macro_rules! outln {
    ($($arg:tt)*) => {{
        use ::std::io::Write;
        let mut stdout = ::std::io::stdout().lock();
        if let Err(e) = writeln!(stdout, $($arg)*) {
            $crate::output::handle_stdout_error(e);
        }
    }};
}

pub(crate) use outln;

/// `eprintln!` that treats a closed stderr as clean process exit, mirroring
/// [`outln!`]'s broken-pipe handling. Used for the interactive/piped shell's
/// error lines, so they land on stderr and never interleave with result
/// output on stdout.
macro_rules! errln {
    ($($arg:tt)*) => {{
        use ::std::io::Write;
        let mut stderr = ::std::io::stderr().lock();
        if let Err(e) = writeln!(stderr, $($arg)*) {
            $crate::output::handle_stderr_error(e);
        }
    }};
}

pub(crate) use errln;

/// Shared failure path for the macro: broken pipe is a quiet success,
/// anything else is a hard error.
///
/// `process::exit` skips destructors, so this must not be reachable
/// while Drop-critical state is live — in particular a held `WriteLock`
/// (ADR-0010), whose leak is a manual-recovery event. Today all
/// transactions are consumed before any `outln!` runs; keep it that way
/// when adding progress output to write commands.
pub(crate) fn handle_stdout_error(error: std::io::Error) -> ! {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("error: cannot write to stdout: {error}");
    std::process::exit(1);
}

/// Shared failure path for [`errln!`]: broken pipe (stderr consumer gone) is a
/// quiet success, anything else is a hard error. Same Drop-safety caveat as
/// [`handle_stdout_error`] — `process::exit` skips destructors.
pub(crate) fn handle_stderr_error(error: std::io::Error) -> ! {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    // Fall back to the (unchecked) stdlib macro; if stderr itself is broken
    // this is best-effort.
    eprintln!("error: cannot write to stderr: {error}");
    std::process::exit(1);
}
