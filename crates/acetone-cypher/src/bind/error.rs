//! Binder errors. Every variant carries a span; `tck_detail` maps a
//! variant to the openCypher TCK's error-detail vocabulary so the TCK
//! harness can verify expected compile-time errors precisely instead of
//! guessing from the fact of rejection.

use crate::span::Span;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BindError {
    #[error("undefined variable '{name}' at bytes {}..{}", span.start, span.end)]
    UndefinedVariable { name: String, span: Span },

    #[error("variable '{name}' is already bound at bytes {}..{}", span.start, span.end)]
    VariableAlreadyBound { name: String, span: Span },

    #[error("variable '{name}' is a {actual}, expected a {expected} at bytes {}..{}", span.start, span.end)]
    VariableTypeConflict {
        name: String,
        expected: &'static str,
        actual: &'static str,
        span: Span,
    },

    #[error("unknown function '{name}' at bytes {}..{}", span.start, span.end)]
    UnknownFunction { name: String, span: Span },

    #[error("wrong number of arguments for '{name}': got {got}, expected {expected} at bytes {}..{}", span.start, span.end)]
    InvalidNumberOfArguments {
        name: String,
        expected: String,
        got: usize,
        span: Span,
    },

    #[error("aggregation is not allowed here at bytes {}..{}", span.start, span.end)]
    InvalidAggregation { span: Span },

    #[error("aggregate functions cannot be nested at bytes {}..{}", span.start, span.end)]
    NestedAggregation { span: Span },

    #[error("duplicate column name '{name}' at bytes {}..{}", span.start, span.end)]
    ColumnNameConflict { name: String, span: Span },

    #[error("RETURN * requires at least one variable in scope at bytes {}..{}", span.start, span.end)]
    NoVariablesInScope { span: Span },

    #[error("expressions in a projection must be aliased before reuse at bytes {}..{}", span.start, span.end)]
    NoExpressionAlias { span: Span },

    #[error("unknown label '{name}' at bytes {}..{} (declared labels come from the schema map)", span.start, span.end)]
    UnknownLabel { name: String, span: Span },

    #[error("unknown relationship type '{name}' at bytes {}..{}", span.start, span.end)]
    UnknownRelType { name: String, span: Span },

    #[error("unknown property '{property}' on label '{label}' at bytes {}..{}", span.start, span.end)]
    UnknownProperty {
        label: String,
        property: String,
        span: Span,
    },

    #[error("unknown procedure '{name}' at bytes {}..{}", span.start, span.end)]
    ProcedureNotFound { name: String, span: Span },

    #[error("procedure '{procedure}' does not yield '{column}' at bytes {}..{}", span.start, span.end)]
    UnknownYieldColumn {
        procedure: String,
        column: String,
        span: Span,
    },

    #[error("pattern predicates cannot introduce new variables ('{name}') at bytes {}..{}", span.start, span.end)]
    NewVariableInPatternPredicate { name: String, span: Span },

    #[error("CREATE requires a directed relationship at bytes {}..{}", span.start, span.end)]
    CreateRequiresDirectedRelationship { span: Span },

    #[error("CREATE requires exactly one relationship type at bytes {}..{}", span.start, span.end)]
    CreateRequiresSingleRelType { span: Span },

    #[error("variable-length relationships cannot be created at bytes {}..{}", span.start, span.end)]
    CreateVarLengthRelationship { span: Span },
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
            | BindError::CreateRequiresSingleRelType { span }
            | BindError::CreateVarLengthRelationship { span } => *span,
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
            BindError::CreateRequiresSingleRelType { .. } => Some("NoSingleRelationshipType"),
            BindError::CreateVarLengthRelationship { .. } => Some("CreatingVarLength"),
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
