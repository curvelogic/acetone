//! Parser errors. Every variant carries a [`Span`] into the query source;
//! none of the front end panics on any input (enforced by fuzz tests).

use crate::span::Span;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("{message} at bytes {}..{}", span.start, span.end)]
    Lex { message: String, span: Span },

    #[error("expected {expected}, found {found} at bytes {}..{}", span.start, span.end)]
    Unexpected {
        expected: String,
        found: String,
        span: Span,
    },

    #[error("'{word}' is a reserved word and cannot be used as {usage} at bytes {}..{} (backquote it to use it as a name)", span.start, span.end)]
    ReservedWord {
        word: String,
        usage: &'static str,
        span: Span,
    },

    #[error("expression nesting exceeds the depth limit ({limit}) at bytes {}..{}", span.start, span.end)]
    RecursionLimit { limit: usize, span: Span },

    #[error("{message} at bytes {}..{}", span.start, span.end)]
    QueryStructure { message: String, span: Span },
}

impl ParseError {
    pub fn span(&self) -> Span {
        match self {
            ParseError::Lex { span, .. }
            | ParseError::Unexpected { span, .. }
            | ParseError::ReservedWord { span, .. }
            | ParseError::RecursionLimit { span, .. }
            | ParseError::QueryStructure { span, .. } => *span,
        }
    }

    /// Render the error with 1-based line/column against the source it
    /// came from, for human-facing output.
    pub fn render(&self, source: &str) -> String {
        let (line, col) = self.span().line_col(source);
        format!("line {line}, column {col}: {self}")
    }
}
