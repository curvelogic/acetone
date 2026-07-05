//! The harness must load the entire vendored corpus: every step matched
//! against the vocabulary, every outline expanded, and the resulting
//! report internally consistent. This is the CI gate — the pass *rate* is
//! published, not gated, but the harness running to completion is
//! mandatory.

use std::path::Path;

use acetone_tck::{classify, run, scenario};

fn features_root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/features"))
}

#[test]
fn whole_corpus_loads_and_classifies() {
    let plans = scenario::load_all(features_root()).expect("corpus must load cleanly");
    // 220 feature files; outline expansion multiplies scenarios well past
    // the raw scenario count. Guard against silent corpus truncation.
    assert!(
        plans.len() > 2_000,
        "expected thousands of scenarios, got {}",
        plans.len()
    );

    let report = run(features_root()).expect("harness must run");
    let t = &report.totals;
    assert_eq!(t.scenarios, plans.len());
    assert_eq!(
        t.scenarios,
        t.passed
            + t.failed
            + t.unsupported_executor
            + t.unsupported_compile_classification
            + t.unsupported_deferred_syntax,
        "every scenario must land in exactly one bucket"
    );
    assert_eq!(report.failures.len(), t.failed);

    // The corpus spans all three top-level areas.
    for area in ["clauses", "expressions", "useCases"] {
        assert!(report.by_area.contains_key(area), "missing area {area}");
    }
}

#[test]
fn report_serialises_and_summarises() {
    let mut report = run(features_root()).expect("harness must run");
    report.parse = classify::parse_stats(&scenario::load_all(features_root()).unwrap());

    let json = serde_json::to_string_pretty(&report).expect("report must serialise");
    assert!(json.contains("tck_commit"));

    let summary = report.summary();
    assert!(summary.contains("openCypher TCK conformance"));
    assert!(summary.contains("parser over queries-under-test"));
}
