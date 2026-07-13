//! Binder errors. Every variant carries a span; `tck_detail` maps a
//! variant to the openCypher TCK's error-detail vocabulary so the TCK
//! harness can verify expected compile-time errors precisely instead of
//! guessing from the fact of rejection.

use crate::span::Span;
use std::fmt;
use thiserror::Error;

/// An optional "did you mean …?" suffix appended to an "unknown X" error.
///
/// It holds the raw closest candidate (or `None`). Its `Display` renders the
/// candidate through [`acetone_model::display::format_label`] — candidates come
/// from the schema catalogue, which a hostile clone can write, so the name is
/// escaped rather than emitted raw. When there is no suggestion it renders as
/// the empty string, so the variant's `#[error(...)]` can interpolate it
/// unconditionally.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Suggestion(pub Option<String>);

impl Suggestion {
    /// No near match.
    pub fn none() -> Self {
        Suggestion(None)
    }
}

impl fmt::Display for Suggestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(name) => {
                write!(
                    f,
                    " — did you mean {}?",
                    acetone_model::display::format_label(name)
                )
            }
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BindError {
    #[error("undefined variable '{name}'")]
    UndefinedVariable { name: String, span: Span },

    #[error("variable '{name}' is already bound")]
    VariableAlreadyBound { name: String, span: Span },

    #[error("variable '{name}' is a {actual}, expected a {expected}")]
    VariableTypeConflict {
        name: String,
        expected: &'static str,
        actual: &'static str,
        span: Span,
    },

    #[error("unknown function '{name}'{suggestion}")]
    UnknownFunction {
        name: String,
        span: Span,
        suggestion: Suggestion,
    },

    #[error("wrong number of arguments for '{name}': got {got}, expected {expected}")]
    InvalidNumberOfArguments {
        name: String,
        expected: String,
        got: usize,
        span: Span,
    },

    #[error("aggregation is not allowed here")]
    InvalidAggregation { span: Span },

    #[error("aggregate functions cannot be nested")]
    NestedAggregation { span: Span },

    #[error("duplicate column name '{name}'")]
    ColumnNameConflict { name: String, span: Span },

    #[error("RETURN * requires at least one variable in scope")]
    NoVariablesInScope { span: Span },

    #[error("expressions in a projection must be aliased before reuse")]
    NoExpressionAlias { span: Span },

    #[error("unknown label '{name}' (declared labels come from the schema map){suggestion}")]
    UnknownLabel {
        name: String,
        span: Span,
        suggestion: Suggestion,
    },

    // The relationship type is echoed into a suggested shell command, so it is
    // escaped (`declare_cmd`) — a backtick-quoted Cypher identifier can carry
    // control characters, which must never reach the terminal raw.
    #[error(
        "unknown relationship type '{name}' — declare it first with `acetone declare-rel-type {declare_cmd}`{suggestion}"
    )]
    UnknownRelType {
        name: String,
        declare_cmd: String,
        span: Span,
        suggestion: Suggestion,
    },

    #[error("unknown property '{property}' on label '{label}'{suggestion}")]
    UnknownProperty {
        label: String,
        property: String,
        span: Span,
        suggestion: Suggestion,
    },

    #[error("unknown procedure '{name}'")]
    ProcedureNotFound { name: String, span: Span },

    #[error("procedure '{procedure}' does not yield '{column}'")]
    UnknownYieldColumn {
        procedure: String,
        column: String,
        span: Span,
    },

    #[error("pattern predicates cannot introduce new variables ('{name}')")]
    NewVariableInPatternPredicate { name: String, span: Span },

    #[error("CREATE requires a directed relationship")]
    CreateRequiresDirectedRelationship { span: Span },

    #[error(
        "CREATE requires a relationship type — write it with a colon, e.g. `[:LINK]`; a bare `[LINK]` is a variable, not a type"
    )]
    CreateRequiresRelType { span: Span },

    #[error("CREATE requires exactly one relationship type")]
    CreateRequiresSingleRelType { span: Span },

    #[error("variable-length relationships cannot be created")]
    CreateVarLengthRelationship { span: Span },

    #[error("cannot create node '{name}' with labels or properties here; it is already bound")]
    CreateBoundNodeWithProperties { name: String, span: Span },

    #[error(
        "cannot modify key property '{property}' of label '{label}' (node identity is immutable; SET/REMOVE must not touch key properties)"
    )]
    SetKeyProperty {
        label: String,
        property: String,
        span: Span,
    },
}

impl BindError {
    pub fn span(&self) -> Span {
        match self {
            BindError::UndefinedVariable { span, .. }
            | BindError::VariableAlreadyBound { span, .. }
            | BindError::VariableTypeConflict { span, .. }
            | BindError::UnknownFunction { span, .. }
            | BindError::InvalidNumberOfArguments { span, .. }
            | BindError::InvalidAggregation { span }
            | BindError::NestedAggregation { span }
            | BindError::ColumnNameConflict { span, .. }
            | BindError::NoVariablesInScope { span }
            | BindError::NoExpressionAlias { span }
            | BindError::UnknownLabel { span, .. }
            | BindError::UnknownRelType { span, .. }
            | BindError::UnknownProperty { span, .. }
            | BindError::ProcedureNotFound { span, .. }
            | BindError::UnknownYieldColumn { span, .. }
            | BindError::NewVariableInPatternPredicate { span, .. }
            | BindError::CreateRequiresDirectedRelationship { span }
            | BindError::CreateRequiresRelType { span }
            | BindError::CreateRequiresSingleRelType { span }
            | BindError::CreateVarLengthRelationship { span }
            | BindError::CreateBoundNodeWithProperties { span, .. }
            | BindError::SetKeyProperty { span, .. } => *span,
        }
    }

    /// The TCK's error-detail name for this error, where one exists. The
    /// TCK phrases these as `SyntaxError: <detail>` (its SyntaxError
    /// covers post-parse compile errors too) except ProcedureNotFound,
    /// which is a `ProcedureError`.
    pub fn tck_detail(&self) -> Option<&'static str> {
        match self {
            BindError::UndefinedVariable { .. }
            | BindError::NewVariableInPatternPredicate { .. } => Some("UndefinedVariable"),
            BindError::VariableAlreadyBound { .. } => Some("VariableAlreadyBound"),
            BindError::VariableTypeConflict { .. } => Some("VariableTypeConflict"),
            BindError::UnknownFunction { .. } => Some("UnknownFunction"),
            BindError::InvalidNumberOfArguments { .. } => Some("InvalidNumberOfArguments"),
            BindError::InvalidAggregation { .. } => Some("InvalidAggregation"),
            BindError::NestedAggregation { .. } => Some("NestedAggregation"),
            BindError::ColumnNameConflict { .. } => Some("ColumnNameConflict"),
            BindError::NoVariablesInScope { .. } => Some("NoVariablesInScope"),
            BindError::NoExpressionAlias { .. } => Some("NoExpressionAlias"),
            BindError::ProcedureNotFound { .. } => Some("ProcedureNotFound"),
            BindError::CreateRequiresDirectedRelationship { .. } => {
                Some("RequiresDirectedRelationship")
            }
            BindError::CreateRequiresRelType { .. }
            | BindError::CreateRequiresSingleRelType { .. } => Some("NoSingleRelationshipType"),
            BindError::CreateVarLengthRelationship { .. } => Some("CreatingVarLength"),
            // openCypher reports attaching labels/properties to an
            // already-bound variable in CREATE as VariableAlreadyBound.
            BindError::CreateBoundNodeWithProperties { .. } => Some("VariableAlreadyBound"),
            // Acetone-specific (openCypher has no key concept), so no TCK
            // detail term.
            BindError::SetKeyProperty { .. } => None,
            BindError::UnknownLabel { .. }
            | BindError::UnknownRelType { .. }
            | BindError::UnknownProperty { .. }
            | BindError::UnknownYieldColumn { .. } => None,
        }
    }

    /// Render with 1-based line/column against the source.
    pub fn render(&self, source: &str) -> String {
        let (line, col) = self.span().line_col(source);
        format!("line {line}, column {col}: {self}")
    }
}
