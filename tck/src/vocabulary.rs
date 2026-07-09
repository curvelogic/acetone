//! The TCK's fixed step vocabulary. Every step in the corpus must match
//! exactly one of these shapes; anything else is a harness error so a
//! corpus bump cannot introduce silently-ignored steps.

/// One recognised TCK step, with its attached payload where relevant.
#[derive(Debug, Clone, PartialEq)]
pub enum StepKind {
    /// `Given an empty graph`
    EmptyGraph,
    /// `Given any graph`
    AnyGraph,
    /// `Given the binary-tree-1 graph` (named fixture graphs)
    NamedGraph(String),
    /// `And having executed:` — a setup query (docstring)
    HavingExecuted(String),
    /// `And parameters are:` — a parameter table
    Parameters,
    /// `Given there exists a procedure ...:` — procedure registration
    ProcedureExists,
    /// `When executing query:` (docstring)
    ExecutingQuery(String),
    /// `When executing control query:` (docstring)
    ExecutingControlQuery(String),
    /// `Then the result should be, in any order:` / `, in order:` /
    /// `(ignoring element order for lists)` variants — a result table
    ExpectResult {
        ordered: bool,
        lists_unordered: bool,
    },
    /// `Then the result should be empty`
    ExpectEmptyResult,
    /// `Then a <Type> should be raised at <phase>: <detail>`
    ExpectError {
        error_type: String,
        phase: ErrorPhase,
        detail: String,
    },
    /// `And no side effects`
    NoSideEffects,
    /// `And the side effects should be:` — a side-effect table
    SideEffects,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPhase {
    CompileTime,
    Runtime,
    AnyTime,
}

/// Match a step's text (with its docstring, if any) against the
/// vocabulary. Returns `None` for unknown steps — the caller escalates.
pub fn match_step(text: &str, docstring: Option<&str>) -> Option<StepKind> {
    let text = text.trim();
    let doc = || docstring.map(str::trim).unwrap_or_default().to_string();

    match text {
        "an empty graph" => return Some(StepKind::EmptyGraph),
        "any graph" => return Some(StepKind::AnyGraph),
        "having executed:" => return Some(StepKind::HavingExecuted(doc())),
        "parameters are:" => return Some(StepKind::Parameters),
        "executing query:" => return Some(StepKind::ExecutingQuery(doc())),
        "executing control query:" => return Some(StepKind::ExecutingControlQuery(doc())),
        "the result should be, in any order:" => {
            return Some(StepKind::ExpectResult {
                ordered: false,
                lists_unordered: false,
            });
        }
        "the result should be, in order:" => {
            return Some(StepKind::ExpectResult {
                ordered: true,
                lists_unordered: false,
            });
        }
        "the result should be (ignoring element order for lists):" => {
            return Some(StepKind::ExpectResult {
                ordered: false,
                lists_unordered: true,
            });
        }
        "the result should be, in order (ignoring element order for lists):" => {
            return Some(StepKind::ExpectResult {
                ordered: true,
                lists_unordered: true,
            });
        }
        "the result should be empty" => return Some(StepKind::ExpectEmptyResult),
        "no side effects" => return Some(StepKind::NoSideEffects),
        "the side effects should be:" => return Some(StepKind::SideEffects),
        _ => {}
    }

    if let Some(rest) = text.strip_prefix("the ")
        && let Some(name) = rest.strip_suffix(" graph")
    {
        return Some(StepKind::NamedGraph(name.to_string()));
    }
    if text.starts_with("there exists a procedure ") {
        return Some(StepKind::ProcedureExists);
    }
    // `a SyntaxError should be raised at compile time: UndefinedVariable`
    if let Some(rest) = text.strip_prefix("a ").or_else(|| text.strip_prefix("an "))
        && let Some(idx) = rest.find(" should be raised at ")
    {
        let error_type = rest[..idx].to_string();
        let tail = &rest[idx + " should be raised at ".len()..];
        let (phase, detail) = if let Some(d) = tail.strip_prefix("compile time:") {
            (ErrorPhase::CompileTime, d)
        } else if let Some(d) = tail.strip_prefix("runtime:") {
            (ErrorPhase::Runtime, d)
        } else {
            // clippy 1.97 `question_mark`: the trailing prefix match returns
            // `None` from the enclosing function if it does not match.
            let d = tail.strip_prefix("any time:")?;
            (ErrorPhase::AnyTime, d)
        };
        return Some(StepKind::ExpectError {
            error_type,
            phase,
            detail: detail.trim().to_string(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_core_vocabulary() {
        assert_eq!(
            match_step("an empty graph", None),
            Some(StepKind::EmptyGraph)
        );
        assert_eq!(
            match_step("the binary-tree-1 graph", None),
            Some(StepKind::NamedGraph("binary-tree-1".into()))
        );
        assert_eq!(
            match_step("executing query:", Some("RETURN 1")),
            Some(StepKind::ExecutingQuery("RETURN 1".into()))
        );
        assert!(matches!(
            match_step("the result should be, in any order:", None),
            Some(StepKind::ExpectResult { ordered: false, .. })
        ));
        assert_eq!(
            match_step(
                "a SyntaxError should be raised at compile time: UndefinedVariable",
                None
            ),
            Some(StepKind::ExpectError {
                error_type: "SyntaxError".into(),
                phase: ErrorPhase::CompileTime,
                detail: "UndefinedVariable".into(),
            })
        );
        assert!(matches!(
            match_step("a TypeError should be raised at any time: *", None),
            Some(StepKind::ExpectError {
                phase: ErrorPhase::AnyTime,
                ..
            })
        ));
    }

    #[test]
    fn unknown_steps_are_none() {
        assert_eq!(match_step("the graph should be frobnicated", None), None);
    }
}
