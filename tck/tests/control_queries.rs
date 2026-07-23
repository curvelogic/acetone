//! Control-query handling (bead acetone-q9m): a `When executing control
//! query:` step is a *follow-up verification* — it runs after the query
//! under test, against the post-write graph, with its own expected
//! table. The harness previously pushed it into the setup queries (so it
//! ran before the write) and let its `Then` table overwrite the main
//! expectation, which made Merge6 [3]/[4] fail with "columns differ".

use std::path::Path;

use acetone_tck::classify::Verdict;
use acetone_tck::{classify, scenario};

fn features_root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/features"))
}

/// The four MERGE-relationship scenarios acetone-q9m fixes: two were
/// control-query misroutes, two were `SET r = <node>` executor rejections.
/// All four must verify end to end now.
#[test]
fn merge_relationship_control_scenarios_pass() {
    let plans = scenario::load_all(features_root()).expect("corpus loads");
    let targets = [
        (
            "clauses/merge/Merge6.feature",
            "[3] Updating one property with ON CREATE",
        ),
        (
            "clauses/merge/Merge6.feature",
            "[4] Null-setting one property with ON CREATE",
        ),
        (
            "clauses/merge/Merge6.feature",
            "[6] Copying properties from node with ON CREATE",
        ),
        (
            "clauses/merge/Merge7.feature",
            "[4] Copying properties from node with ON MATCH",
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

/// Reduction routes the control query and its `Then` table to a separate
/// check, leaving the main expectation (and side effects) intact.
#[test]
fn control_query_reduces_to_its_own_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Control.feature"),
        r#"Feature: Control - control query routing

  Scenario: [1] Control query verifies after the write
    Given an empty graph
    When executing query:
      """
      CREATE ({name: 'A'})
      """
    Then the result should be empty
    And the side effects should be:
      | +nodes      | 1 |
      | +properties | 1 |
    When executing control query:
      """
      MATCH (n)
      RETURN n.name
      """
    Then the result should be, in any order:
      | n.name |
      | 'A'    |
"#,
    )
    .expect("write feature");

    let plans = scenario::load_all(dir.path()).expect("synthetic corpus loads");
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];

    assert_eq!(plan.expectation, scenario::Expectation::EmptyResult);
    assert!(
        plan.setup_queries.is_empty(),
        "the control query must not run as setup: {:?}",
        plan.setup_queries
    );
    assert_eq!(plan.controls.len(), 1);
    assert!(plan.controls[0].query.contains("RETURN n.name"));
    match &plan.controls[0].expectation {
        scenario::Expectation::Rows { header, rows, .. } => {
            assert_eq!(header, &["n.name".to_string()]);
            assert_eq!(rows, &[vec!["'A'".to_string()]]);
        }
        other => panic!("control expectation must be the Then table, got {other:?}"),
    }

    assert_eq!(classify(plan), Verdict::Passed);
}

/// A control query that disproves the write is a Failed verdict — the
/// check really runs, it is not silently dropped.
#[test]
fn control_query_mismatch_is_a_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Control.feature"),
        r#"Feature: Control - control query mismatch

  Scenario: [1] Control query catches a wrong write
    Given an empty graph
    When executing query:
      """
      CREATE ({name: 'A'})
      """
    Then the result should be empty
    When executing control query:
      """
      MATCH (n)
      RETURN n.name
      """
    Then the result should be, in any order:
      | n.name |
      | 'B'    |
"#,
    )
    .expect("write feature");

    let plans = scenario::load_all(dir.path()).expect("synthetic corpus loads");
    assert_eq!(plans.len(), 1);
    match classify(&plans[0]) {
        Verdict::Failed { reason } => {
            assert!(
                reason.contains("control"),
                "reason names the control query: {reason}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}
