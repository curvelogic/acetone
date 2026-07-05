//! Corpus loading: walk the vendored feature tree, parse Gherkin, expand
//! scenario outlines against their Examples tables, prepend Background
//! steps, and reduce every scenario to a [`ScenarioPlan`].

use std::fmt;
use std::path::{Path, PathBuf};

use crate::vocabulary::{ErrorPhase, StepKind, match_step};

#[derive(Debug)]
pub enum HarnessError {
    Io {
        path: PathBuf,
        message: String,
    },
    Gherkin {
        path: PathBuf,
        message: String,
    },
    UnknownStep {
        path: PathBuf,
        scenario: String,
        step: String,
    },
    Outline {
        path: PathBuf,
        scenario: String,
        message: String,
    },
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HarnessError::Io { path, message } => {
                write!(f, "cannot read {}: {message}", path.display())
            }
            HarnessError::Gherkin { path, message } => {
                write!(f, "cannot parse {}: {message}", path.display())
            }
            HarnessError::UnknownStep {
                path,
                scenario,
                step,
            } => write!(
                f,
                "unknown TCK step in {} / {scenario:?}: {step:?} — extend the vocabulary deliberately",
                path.display()
            ),
            HarnessError::Outline {
                path,
                scenario,
                message,
            } => {
                write!(
                    f,
                    "bad scenario outline in {} / {scenario:?}: {message}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for HarnessError {}

/// What one scenario needs and expects, reduced from its steps.
#[derive(Debug)]
pub struct ScenarioPlan {
    /// Path relative to the features root, e.g. `clauses/match/Match1.feature`.
    pub feature_path: String,
    pub scenario_name: String,
    /// Setup queries (`having executed:`, control queries).
    pub setup_queries: Vec<String>,
    /// True when the scenario needs a pre-built named fixture graph.
    pub named_graph: Option<String>,
    /// True when the scenario supplies query parameters.
    pub has_parameters: bool,
    /// True when the scenario registers stub procedures.
    pub needs_procedures: bool,
    /// The query under test.
    pub query: Option<String>,
    pub expectation: Expectation,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expectation {
    /// A result table: header, raw cell texts, and ordering semantics.
    Rows {
        header: Vec<String>,
        rows: Vec<Vec<String>>,
        ordered: bool,
        lists_unordered: bool,
    },
    EmptyResult,
    Error {
        error_type: String,
        phase: ErrorPhase,
        detail: String,
    },
    /// The scenario declares no `Then` outcome the harness models.
    None,
}

/// Load and reduce the whole corpus.
pub fn load_all(features_root: &Path) -> Result<Vec<ScenarioPlan>, HarnessError> {
    let mut feature_files = Vec::new();
    collect_features(features_root, &mut feature_files)?;
    feature_files.sort();

    let mut plans = Vec::new();
    for path in feature_files {
        let text = std::fs::read_to_string(&path).map_err(|e| HarnessError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let text = normalise_leading_and(&text);
        let feature =
            gherkin::Feature::parse(&text, gherkin::GherkinEnv::default()).map_err(|e| {
                HarnessError::Gherkin {
                    path: path.clone(),
                    message: e.to_string(),
                }
            })?;
        let rel = path
            .strip_prefix(features_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        reduce_feature(&feature, &path, &rel, &mut plans)?;
    }
    Ok(plans)
}

/// The upstream corpus sometimes opens a scenario with `And ...` (leaning
/// on context the reader infers); the strict gherkin parser requires the
/// first step to be Given/When/Then. Normalise in memory — the vendored
/// files are never edited. The keyword carries no semantics for this
/// harness: steps are matched by their text, not their keyword.
pub fn normalise_leading_and(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut expecting_first_step = false;
    let mut in_docstring = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        // Never rewrite inside `"""` docstrings: a future corpus bump
        // whose query text happens to contain step-shaped lines must not
        // be silently corrupted.
        if trimmed.starts_with("\"\"\"") {
            in_docstring = !in_docstring;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_docstring {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if trimmed.starts_with("Scenario:")
            || trimmed.starts_with("Scenario Outline:")
            || trimmed.starts_with("Background:")
        {
            expecting_first_step = true;
        } else if expecting_first_step {
            if let Some(rest) = trimmed.strip_prefix("And ") {
                let indent = &line[..line.len() - trimmed.len()];
                out.push_str(indent);
                out.push_str("Given ");
                out.push_str(rest);
                out.push('\n');
                expecting_first_step = false;
                continue;
            }
            // Any non-blank, non-tag, non-comment line settles the question.
            if !trimmed.is_empty() && !trimmed.starts_with('#') && !trimmed.starts_with('@') {
                expecting_first_step = false;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn collect_features(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), HarnessError> {
    let entries = std::fs::read_dir(dir).map_err(|e| HarnessError::Io {
        path: dir.to_path_buf(),
        message: e.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| HarnessError::Io {
            path: dir.to_path_buf(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_features(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "feature") {
            out.push(path);
        }
    }
    Ok(())
}

fn reduce_feature(
    feature: &gherkin::Feature,
    path: &Path,
    rel: &str,
    out: &mut Vec<ScenarioPlan>,
) -> Result<(), HarnessError> {
    let background: &[gherkin::Step] = feature
        .background
        .as_ref()
        .map(|b| b.steps.as_slice())
        .unwrap_or_default();

    for scenario in &feature.scenarios {
        let mut all_steps: Vec<gherkin::Step> = background.to_vec();
        all_steps.extend(scenario.steps.iter().cloned());

        if scenario.examples.is_empty() {
            out.push(reduce_scenario(path, rel, &scenario.name, &all_steps)?);
            continue;
        }
        // Scenario Outline: expand each Examples row into a concrete
        // scenario by substituting <column> placeholders.
        for examples in &scenario.examples {
            // Malformed Examples are harness errors, never silent skips.
            let Some(table) = examples.table.as_ref() else {
                return Err(HarnessError::Outline {
                    path: path.to_path_buf(),
                    scenario: scenario.name.clone(),
                    message: "Examples block without a table".into(),
                });
            };
            let Some((header, rows)) = table.rows.split_first() else {
                return Err(HarnessError::Outline {
                    path: path.to_path_buf(),
                    scenario: scenario.name.clone(),
                    message: "Examples table without a header row".into(),
                });
            };
            for (row_index, row) in rows.iter().enumerate() {
                if row.len() != header.len() {
                    return Err(HarnessError::Outline {
                        path: path.to_path_buf(),
                        scenario: scenario.name.clone(),
                        message: format!("examples row {} width mismatch", row_index + 1),
                    });
                }
                let substituted: Vec<gherkin::Step> = all_steps
                    .iter()
                    .map(|step| substitute_step(step, header, row))
                    .collect();
                let name = format!("{} #{}", scenario.name, row_index + 1);
                out.push(reduce_scenario(path, rel, &name, &substituted)?);
            }
        }
    }
    Ok(())
}

fn substitute_step(step: &gherkin::Step, header: &[String], row: &[String]) -> gherkin::Step {
    let apply = |text: &str| -> String {
        let mut text = text.to_string();
        for (column, value) in header.iter().zip(row) {
            text = text.replace(&format!("<{column}>"), value);
        }
        text
    };
    let mut step = step.clone();
    step.value = apply(&step.value);
    step.docstring = step.docstring.as_deref().map(apply);
    if let Some(table) = &mut step.table {
        for table_row in &mut table.rows {
            for cell in table_row {
                *cell = apply(cell);
            }
        }
    }
    step
}

fn reduce_scenario(
    path: &Path,
    rel: &str,
    name: &str,
    steps: &[gherkin::Step],
) -> Result<ScenarioPlan, HarnessError> {
    let mut plan = ScenarioPlan {
        feature_path: rel.to_string(),
        scenario_name: name.to_string(),
        setup_queries: Vec::new(),
        named_graph: None,
        has_parameters: false,
        needs_procedures: false,
        query: None,
        expectation: Expectation::None,
    };

    for step in steps {
        let kind = match_step(&step.value, step.docstring.as_deref()).ok_or_else(|| {
            HarnessError::UnknownStep {
                path: path.to_path_buf(),
                scenario: name.to_string(),
                step: step.value.clone(),
            }
        })?;
        match kind {
            StepKind::EmptyGraph | StepKind::AnyGraph => {}
            StepKind::NamedGraph(graph) => plan.named_graph = Some(graph),
            StepKind::HavingExecuted(query) => plan.setup_queries.push(query),
            StepKind::Parameters => plan.has_parameters = true,
            StepKind::ProcedureExists => plan.needs_procedures = true,
            StepKind::ExecutingQuery(query) => plan.query = Some(query),
            StepKind::ExecutingControlQuery(query) => plan.setup_queries.push(query),
            StepKind::ExpectResult {
                ordered,
                lists_unordered,
            } => {
                let (header, rows) = match &step.table {
                    Some(table) => match table.rows.split_first() {
                        Some((header, rows)) => (header.clone(), rows.to_vec()),
                        None => (Vec::new(), Vec::new()),
                    },
                    None => (Vec::new(), Vec::new()),
                };
                plan.expectation = Expectation::Rows {
                    header,
                    rows,
                    ordered,
                    lists_unordered,
                };
            }
            StepKind::ExpectEmptyResult => plan.expectation = Expectation::EmptyResult,
            StepKind::ExpectError {
                error_type,
                phase,
                detail,
            } => {
                plan.expectation = Expectation::Error {
                    error_type,
                    phase,
                    detail,
                };
            }
            StepKind::NoSideEffects | StepKind::SideEffects => {}
        }
    }
    Ok(plan)
}
