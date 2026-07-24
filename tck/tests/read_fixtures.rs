//! Read scenarios over built fixtures (bead acetone-cbl.2): a read query
//! whose scenario carries `And having executed:` setup steps must run
//! against the fixture graph those steps build — the same machinery the
//! write path uses — rather than sitting in the unsupported-executor
//! bucket. Row verification and the "no side effects" assertion stay
//! load-bearing: a wrong table is Failed, never quietly credited.

use std::path::Path;

use acetone_tck::classify::Verdict;
use acetone_tck::{classify, scenario};

fn features_root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/features"))
}

/// Representative read-with-setup scenarios the engine fully supports:
/// they must verify end to end now that the fixture is built for reads.
#[test]
fn read_scenarios_with_setup_pass() {
    let plans = scenario::load_all(features_root()).expect("corpus loads");
    let targets = [
        ("clauses/match/Match1.feature", "[2] Matching all nodes"),
        (
            "clauses/match/Match1.feature",
            "[3] Matching nodes using multiple labels",
        ),
        (
            "clauses/match-where/MatchWhere1.feature",
            "[3] Filter node with property predicate on a single variable with multiple bindings",
        ),
    ];
    for (feature, name) in targets {
        let plan = plans
            .iter()
            .find(|p| p.feature_path == feature && p.scenario_name == name)
            .unwrap_or_else(|| panic!("scenario not found: {feature} / {name}"));
        let verdict = classify(plan);
        assert_eq!(
            verdict,
            Verdict::Passed,
            "{feature} / {name} must verify end to end, got {verdict:?}"
        );
    }
}

/// Gaming resistance: the fixture path must actually verify the rows — a
/// scenario whose expected table is wrong is Failed, not credited.
#[test]
fn read_scenario_with_wrong_table_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Synthetic.feature"),
        r#"Feature: Synthetic - read fixture verification

  Scenario: [1] Wrong expected table must fail
    Given an empty graph
    And having executed:
      """
      CREATE ({name: 'only'})
      """
    When executing query:
      """
      MATCH (n)
      RETURN n.name
      """
    Then the result should be, in any order:
      | n.name      |
      | 'different' |
    And no side effects
"#,
    )
    .expect("write feature");
    let plans = scenario::load_all(dir.path()).expect("synthetic corpus loads");
    assert_eq!(plans.len(), 1);
    let verdict = classify(&plans[0]);
    assert!(
        matches!(verdict, Verdict::Failed { .. }),
        "wrong table must be Failed, got {verdict:?}"
    );
}

/// A read scenario whose *setup* the engine cannot build stays
/// unsupported — an unbuildable fixture says nothing about the query.
#[test]
fn read_scenario_with_unbuildable_setup_stays_unsupported() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Synthetic.feature"),
        r#"Feature: Synthetic - unbuildable fixture

  Scenario: [1] FOREACH setup is beyond the engine
    Given an empty graph
    And having executed:
      """
      FOREACH (i IN [1, 2] | CREATE ({n: i}))
      """
    When executing query:
      """
      MATCH (n)
      RETURN n.n
      """
    Then the result should be, in any order:
      | n.n |
      | 1   |
      | 2   |
    And no side effects
"#,
    )
    .expect("write feature");
    let plans = scenario::load_all(dir.path()).expect("synthetic corpus loads");
    assert_eq!(plans.len(), 1);
    let verdict = classify(&plans[0]);
    assert!(
        matches!(verdict, Verdict::Unsupported(_)),
        "unbuildable fixture must stay unsupported, got {verdict:?}"
    );
}
