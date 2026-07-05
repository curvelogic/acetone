//! Runs the vendored TCK and emits the conformance report.
//!
//! Usage: `tck-runner [--features <dir>] [--report <file.json>]`
//!
//! Exit code 0 means the harness ran to completion — the pass rate is a
//! published number, not a gate (Gate C sets the bar at the Phase 2
//! boundary). Non-zero means the harness itself failed (unreadable
//! corpus, unknown step vocabulary), which IS a CI failure.

use std::path::PathBuf;
use std::process::ExitCode;

use acetone_tck::run;

fn main() -> ExitCode {
    let mut features = PathBuf::from("tck/features");
    let mut report_path: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--features" => match args.next() {
                Some(dir) => features = PathBuf::from(dir),
                None => return usage("--features needs a directory"),
            },
            "--report" => match args.next() {
                Some(file) => report_path = Some(PathBuf::from(file)),
                None => return usage("--report needs a file path"),
            },
            other => return usage(&format!("unknown argument {other:?}")),
        }
    }

    let report = match run(&features) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("tck harness error: {e}");
            return ExitCode::FAILURE;
        }
    };

    print!("{}", report.summary());

    if let Some(path) = report_path {
        let json = match serde_json::to_string_pretty(&report) {
            Ok(json) => json,
            Err(e) => {
                eprintln!("cannot serialise report: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = std::fs::write(&path, json) {
            eprintln!("cannot write {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        println!("report written to {}", path.display());
    }
    ExitCode::SUCCESS
}

fn usage(problem: &str) -> ExitCode {
    eprintln!("{problem}\nusage: tck_runner [--features <dir>] [--report <file.json>]");
    ExitCode::FAILURE
}
