//! Panic-free stdout for a CLI that lives in pipelines.
//!
//! `println!` panics when the consumer closes the pipe early
//! (`acetone log | grep -q ...`, `| head`). For a CLI that is normal
//! termination, not an error: exit 0 quietly. Exiting 0 rather than
//! re-raising SIGPIPE keeps `pipefail` scripts working and avoids an
//! `unsafe` libc dependency. Any other stdout failure is real and exits 1.

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

/// Shared failure path for the macro: broken pipe is a quiet success,
/// anything else is a hard error.
pub(crate) fn handle_stdout_error(error: std::io::Error) -> ! {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("error: cannot write to stdout: {error}");
    std::process::exit(1);
}
