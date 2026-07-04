//! Spike B: a hand-rolled recursive-descent openCypher parser slice.
//!
//! Purpose: gauge the real cost and shape of the "vendor the grammar"
//! option — own lexer, own spanned AST, Pratt expression parsing, the
//! grammar's genuinely awkward corners (pattern predicates vs
//! parenthesised expressions, list comprehensions vs list literals), and
//! the `AT <ref>` acetone extension that no off-the-shelf parser accepts.
//!
//! This is spike code: it favours brevity over polish, but the shape
//! (spans everywhere, error type with position, no panics on any input)
//! is the shape the real parser would have.

pub mod ast;
pub mod lexer;
pub mod parser;

pub use parser::parse;
