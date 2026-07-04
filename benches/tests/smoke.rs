//! CI smoke test: run the full scenario set at a tiny scale and assert every
//! structural invariant. Fast (a couple of seconds) so it does not materially
//! slow `cargo test --workspace`; the full-scale numbers are a manual run of
//! the `bench` binary (see README.md).

use acetone_bench::{Config, run};

#[test]
fn smoke_runs_all_scenarios_and_holds_invariants() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = Config::smoke(dir.path().join("repo"));
    let n = cfg.n;
    let samples = cfg.samples;

    let report = run(cfg, None).expect("benchmark run");

    // Bulk load produced a real multi-level tree and wrote chunks.
    assert!(report.height >= 1, "tree height must be at least 1");
    assert!(report.load_chunks > 0, "bulk load must write chunks");

    // A full scan saw every key.
    assert_eq!(report.scan_count, n, "full scan must visit every key");

    // Single-key update amplification stayed within the spine bound.
    assert!(
        report.update_amp_max <= report.height as u64 + 2,
        "update amplification {} exceeds height {} + 2",
        report.update_amp_max,
        report.height
    );

    // diff reported the exact churned set, agreed with a naive scan diff, and
    // history independence (Load-Bearing Invariant 1) held.
    assert!(report.diff_changes > 0, "1% churn must produce changes");
    assert_eq!(
        report.diff_matches_expected,
        Some(true),
        "diff must match the intended churn set"
    );
    assert_eq!(
        report.diff_matches_scan,
        Some(true),
        "diff must match a naive scan-based diff"
    );
    assert_eq!(
        report.history_independent,
        Some(true),
        "apply_batch root must equal bulk_load root (history independence)"
    );

    // Growth committed every import and the head reconstructed and read back.
    assert!(report.growth_commits > 0, "growth must apply commits");
    assert!(
        report.growth_readback.is_some_and(|c| c > 0),
        "head must read back after growth"
    );

    // Packed point reads (post-gc) resolved the same sample, if git was
    // available; when git gc is unavailable the scenario reports None.
    if let Some(found) = report.packed_reads_found {
        assert_eq!(
            found, samples,
            "every sampled key must resolve after repack"
        );
    }
}

#[test]
fn scenario_subset_still_bulk_loads_first() {
    // Requesting only `scan` must still work: bulk-load always runs first.
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = Config::smoke(dir.path().join("repo"));
    let n = cfg.n;
    let report = run(cfg, Some(&["scan".to_string()])).expect("scan-only run");
    assert_eq!(
        report.scan_count, n,
        "scan-only run must still see every key"
    );
    // Scenarios not requested left their report fields at defaults.
    assert_eq!(report.diff_changes, 0, "diff must not have run");
}
