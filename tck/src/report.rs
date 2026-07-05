//! The conformance report: JSON for machines (published as a CI artefact
//! per commit), a text summary for humans.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::classify::{ParseStats, UnsupportedReason, Verdict};
use crate::scenario::ScenarioPlan;

/// The pinned upstream commit of the vendored corpus; keep in step with
/// tck/features/README.md when bumping.
pub const TCK_COMMIT: &str = "677cbafabb8c3c5eed458fd3b1ec0daec8d67d23";

#[derive(Debug, Default, Clone, Serialize)]
pub struct Counts {
    pub scenarios: usize,
    pub passed: usize,
    pub failed: usize,
    pub unsupported_executor: usize,
    pub unsupported_compile_classification: usize,
    pub unsupported_deferred_syntax: usize,
}

impl Counts {
    fn record(&mut self, verdict: &Verdict) {
        self.scenarios += 1;
        match verdict {
            Verdict::Passed => self.passed += 1,
            Verdict::Failed { .. } => self.failed += 1,
            Verdict::Unsupported(UnsupportedReason::Executor) => self.unsupported_executor += 1,
            Verdict::Unsupported(UnsupportedReason::CompileClassification) => {
                self.unsupported_compile_classification += 1;
            }
            Verdict::Unsupported(UnsupportedReason::DeferredSyntax) => {
                self.unsupported_deferred_syntax += 1;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub feature: String,
    pub scenario: String,
    pub query: Option<String>,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub tck_commit: &'static str,
    pub totals: Counts,
    /// Counts keyed by top-level TCK area (`clauses`, `expressions`,
    /// `useCases`).
    pub by_area: BTreeMap<String, Counts>,
    /// Every Failed scenario, in corpus order — the actionable list.
    pub failures: Vec<Failure>,
    /// Parser acceptance over the corpus's queries-under-test.
    pub parse: ParseStats,
}

impl Report {
    pub fn new(_expected_scenarios: usize) -> Self {
        Report {
            tck_commit: TCK_COMMIT,
            totals: Counts::default(),
            by_area: BTreeMap::new(),
            failures: Vec::new(),
            parse: ParseStats::default(),
        }
    }

    pub fn record(&mut self, plan: &ScenarioPlan, verdict: &Verdict) {
        self.totals.record(verdict);
        let area = plan
            .feature_path
            .split('/')
            .next()
            .unwrap_or("other")
            .to_string();
        self.by_area.entry(area).or_default().record(verdict);
        if let Verdict::Failed { reason } = verdict {
            self.failures.push(Failure {
                feature: plan.feature_path.clone(),
                scenario: plan.scenario_name.clone(),
                query: plan.query.clone(),
                reason: reason.clone(),
            });
        }
    }

    /// Human-readable summary.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        let t = &self.totals;
        let pct = |n: usize| {
            if t.scenarios == 0 {
                0.0
            } else {
                100.0 * n as f64 / t.scenarios as f64
            }
        };
        out.push_str(&format!(
            "openCypher TCK conformance (corpus {} scenarios, upstream {})\n",
            t.scenarios,
            &TCK_COMMIT[..12]
        ));
        out.push_str(&format!(
            "  passed:      {:5}  ({:.1}%)\n",
            t.passed,
            pct(t.passed)
        ));
        out.push_str(&format!(
            "  failed:      {:5}  ({:.1}%)\n",
            t.failed,
            pct(t.failed)
        ));
        out.push_str(&format!(
            "  unsupported: {:5}  (executor {} / compile-classification {} / deferred syntax {})\n",
            t.unsupported_executor
                + t.unsupported_compile_classification
                + t.unsupported_deferred_syntax,
            t.unsupported_executor,
            t.unsupported_compile_classification,
            t.unsupported_deferred_syntax,
        ));
        out.push_str(&format!(
            "  parser over queries-under-test: {}/{} parse ({} deferred-syntax rejects, {} other rejects)\n",
            self.parse.parse_ok,
            self.parse.queries,
            self.parse.parse_err_deferred,
            self.parse.parse_err_other,
        ));
        out.push_str("  by area:\n");
        for (area, counts) in &self.by_area {
            out.push_str(&format!(
                "    {:12} {:5} scenarios, {:4} passed, {:3} failed\n",
                area, counts.scenarios, counts.passed, counts.failed
            ));
        }
        if !self.failures.is_empty() {
            out.push_str(&format!("  failures ({}):\n", self.failures.len()));
            for failure in self.failures.iter().take(50) {
                out.push_str(&format!(
                    "    {} / {}\n      {}\n",
                    failure.feature, failure.scenario, failure.reason
                ));
            }
            if self.failures.len() > 50 {
                out.push_str(&format!(
                    "    ... and {} more (see the JSON report)\n",
                    self.failures.len() - 50
                ));
            }
        }
        out
    }
}
