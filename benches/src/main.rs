//! CLI front end for the Phase 0 regression benchmark suite (bead
//! acetone-63m.9). See the crate docs and `benches/README.md`.
//!
//! ```text
//! bench --keys <100k|1m|5m|N> --dir <DIR> [--scenarios a,b,c]
//!       [--samples N] [--windows N] [--window-size N] [--updates N]
//!       [--growth-commits N] [--verify] [--quiet]
//! bench --smoke [--dir <DIR>]        # tiny, asserts every invariant
//! ```
//!
//! The repository is created fresh under `<DIR>/repo-<keys>`; the harness
//! refuses to reuse an existing one. Timing figures print to stdout; the
//! structural assertions run inline and abort on violation.

use std::path::PathBuf;
use std::process::ExitCode;

use acetone_bench::{ALL_SCENARIOS, Config, TempRepo, run};

fn parse_keys(s: &str) -> Result<u64, String> {
    match s.to_ascii_lowercase().as_str() {
        "100k" => Ok(100_000),
        "1m" => Ok(1_000_000),
        "5m" => Ok(5_000_000),
        other => other
            .parse()
            .map_err(|_| format!("--keys must be 100k|1m|5m or a number, got {s}")),
    }
}

fn usage() -> &'static str {
    "usage: bench --keys <100k|1m|5m|N> --dir <DIR> [--scenarios a,b,c] \
     [--samples N] [--windows N] [--window-size N] [--updates N] \
     [--growth-commits N] [--verify] [--quiet]\n       bench --smoke [--dir <DIR>]"
}

fn real_main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut keys: Option<u64> = None;
    let mut dir: Option<PathBuf> = None;
    let mut scenarios: Option<Vec<String>> = None;
    let mut samples = 1000usize;
    let mut windows = 100usize;
    let mut window_size = 1000usize;
    let mut updates = 100usize;
    let mut growth_commits = 100usize;
    let mut smoke = false;
    let mut verify: Option<bool> = None;
    let mut quiet = false;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("missing value for {arg}"))
        };
        match arg.as_str() {
            "--keys" => keys = Some(parse_keys(&val()?)?),
            "--dir" => dir = Some(PathBuf::from(val()?)),
            "--scenarios" => {
                scenarios = Some(val()?.split(',').map(|s| s.trim().to_string()).collect())
            }
            "--samples" => samples = val()?.parse().map_err(|_| "--samples: not a number")?,
            "--windows" => windows = val()?.parse().map_err(|_| "--windows: not a number")?,
            "--window-size" => {
                window_size = val()?.parse().map_err(|_| "--window-size: not a number")?
            }
            "--updates" => updates = val()?.parse().map_err(|_| "--updates: not a number")?,
            "--growth-commits" => {
                growth_commits = val()?
                    .parse()
                    .map_err(|_| "--growth-commits: not a number")?
            }
            "--smoke" => smoke = true,
            "--verify" => verify = Some(true),
            "--no-verify" => verify = Some(false),
            "--quiet" => quiet = true,
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }

    if smoke {
        let base = dir.unwrap_or_else(std::env::temp_dir);
        let repo = base.join(format!("acetone-bench-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        // Guard cleans the repo up on drop even if `run` returns early.
        let _guard = TempRepo::new(repo.clone());
        let mut cfg = Config::smoke(repo);
        cfg.quiet = quiet;
        let report = run(cfg, scenarios.as_deref()).map_err(|e| e.to_string())?;
        println!("\nsmoke report: {report:?}");
        return Ok(());
    }

    let n = keys.ok_or_else(|| format!("--keys is required\n{}", usage()))?;
    let dir = dir.ok_or_else(|| format!("--dir is required\n{}", usage()))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create --dir: {e}"))?;

    // The O(n) cross-checks (naive scan diff, history-independence rebuild)
    // are cheap only at small scale; default them on up to 200k keys, matching
    // the spike, and let --verify/--no-verify override.
    let verify = verify.unwrap_or(n <= 200_000);

    let cfg = Config {
        n,
        repo: dir.join(format!("repo-{n}")),
        samples,
        windows,
        window_size,
        updates,
        growth_commits,
        verify,
        // Full-scale runs sample randomly; the amplification envelope is
        // print-and-warn, not a hard assert (the deterministic smoke run is
        // the gate). See the `update` scenario.
        strict_amp: false,
        quiet,
    };

    // The roadmap pins the growth scenario to the 1M-key graph; above 2M keys
    // it is skipped from the default set (pass --scenarios growth to force).
    let default: Vec<String> = ALL_SCENARIOS
        .iter()
        .filter(|s| **s != "growth" || n <= 2_000_000)
        .map(|s| s.to_string())
        .collect();
    let scenarios = scenarios.unwrap_or(default);

    println!("# acetone-bench: keys={n} repo={}", cfg.repo.display());
    println!("scenarios: {}", scenarios.join(", "));
    run(cfg, Some(&scenarios)).map_err(|e| e.to_string())?;
    Ok(())
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
