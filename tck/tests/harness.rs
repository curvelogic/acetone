//! The harness must load the entire vendored corpus: every step matched
//! against the vocabulary, every outline expanded, and the resulting
//! report internally consistent. This is the CI gate — the pass *rate* is
//! published, not gated, but the harness running to completion is
//! mandatory.

use std::path::Path;

use acetone_tck::{run, scenario};

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
    let report = run(features_root()).expect("harness must run");
    assert!(report.parse.queries > 3_000, "run() must fill parse stats");

    let json = serde_json::to_string_pretty(&report).expect("report must serialise");
    assert!(json.contains("tck_commit"));

    let summary = report.summary();
    assert!(summary.contains("openCypher TCK conformance"));
    assert!(summary.contains("parser over queries-under-test"));
}

/// Pins the leading-`And` normalisation to the pinned corpus: exactly the
/// five genuine step lines in Match5.feature are rewritten, and docstring
/// content is never touched. A corpus bump that changes this count fails
/// here and gets a deliberate look, honouring the features README's
/// "fails loudly" promise.
#[test]
fn leading_and_normalisation_touches_exactly_the_known_sites() {
    let mut rewritten: Vec<(String, usize)> = Vec::new();
    for entry in walk(features_root()) {
        let text = std::fs::read_to_string(&entry).unwrap();
        let normalised = scenario::normalise_leading_and(&text);
        for (index, (before, after)) in text.lines().zip(normalised.lines()).enumerate() {
            if before != after {
                let rel = entry.strip_prefix(features_root()).unwrap();
                rewritten.push((rel.to_string_lossy().into_owned(), index + 1));
            }
        }
    }
    rewritten.sort();
    assert_eq!(
        rewritten,
        vec![
            ("clauses/match/Match5.feature".to_string(), 466),
            ("clauses/match/Match5.feature".to_string(), 501),
            ("clauses/match/Match5.feature".to_string(), 543),
            ("clauses/match/Match5.feature".to_string(), 585),
            ("clauses/match/Match5.feature".to_string(), 620),
        ]
    );
}

#[test]
fn normalisation_never_rewrites_docstring_content() {
    let text = "Feature: f\n  Scenario: s\n    When executing query:\n      \"\"\"\n      Scenario: fake\n      And having executed: not a step\n      \"\"\"\n    Then the result should be empty\n";
    assert_eq!(scenario::normalise_leading_and(text), text);
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else if path.extension().is_some_and(|e| e == "feature") {
            out.push(path);
        }
    }
    out
}
