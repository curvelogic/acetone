//! The openCypher TCK harness (bead acetone-yzc.3).
//!
//! Loads the vendored TCK feature corpus (`tck/features/**`, pinned — see
//! its README), expands scenario outlines, matches every step against the
//! TCK's fixed vocabulary (unknown steps are harness errors, so a corpus
//! bump cannot silently skip anything), and classifies each scenario
//! honestly:
//!
//! - **Passed** — verified end to end. With the current parse-only
//!   backend that means compile-time-`SyntaxError` scenarios whose query
//!   acetone rejects at parse time.
//! - **Failed** — acetone demonstrably disagrees with the TCK. Parse-only
//!   this means a query the TCK requires to be valid (or to fail only at
//!   runtime) that our parser rejects, excluding deferred syntax.
//! - **Unsupported** — not yet decidable, split by what is missing:
//!   the executor (most scenarios), compile-time error classification
//!   (needs the binder), or deferred syntax (spec §5.1 deferrals and the
//!   Phase 3 write clauses).
//!
//! The pass rate is published per commit as a CI artefact, however low —
//! spec §5.1: "Each release MUST publish its TCK pass rate."

pub mod classify;
pub mod report;
pub mod scenario;
pub mod vocabulary;

use std::path::Path;

pub use classify::{Verdict, classify};
pub use report::Report;
pub use scenario::{HarnessError, ScenarioPlan};

/// Load every feature file under `features_root`, expand outlines, and
/// classify all scenarios into a report. Harness-level problems (corpus
/// unreadable, unknown step vocabulary, malformed outline) are errors,
/// never silent skips.
pub fn run(features_root: &Path) -> Result<Report, HarnessError> {
    let plans = scenario::load_all(features_root)?;
    let mut report = Report::new(plans.len());
    for plan in &plans {
        let verdict = classify(plan);
        report.record(plan, &verdict);
    }
    Ok(report)
}
