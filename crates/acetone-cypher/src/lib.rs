//! The openCypher query layer (spec §5).
//!
//! Parser front end producing a spanned AST, binder against the schema map,
//! heuristic logical planning, and a Volcano-style iterator executor over
//! the storage traits. Conformance is measured against the openCypher TCK
//! and the pass rate is published per release.
//!
//! Current surface: the parser (spec §5.1 Level R read subset plus the
//! `AT <ref>` extension and `CALL ... YIELD` of §5.2) and the binder
//! (name resolution, scoping and aggregation validation against a schema
//! catalogue, lowering to the bound IR). Planner and executor follow
//! under Phase 2.

pub mod ast;
pub mod bind;
pub mod error;
pub mod lex;
pub mod parser;
pub mod span;

pub use error::ParseError;
pub use parser::parse;
pub use span::Span;
