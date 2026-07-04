//! Typed, source-spanned AST for the v0.1 read subset (spec §5.1 Level R)
//! plus the acetone versioning surface (spec §5.2): `AT <ref>` on MATCH
//! clause groups and `CALL acetone.*` procedures.
//!
//! Every node carries a [`Span`] so the binder, planner and error paths
//! can always point back into the query text. Write clauses (Level W)
//! arrive with the Phase 3 write path; the [`Clause`] enum is deliberately
//! open for that extension.

use crate::span::Span;

/// A single openCypher query: a sequence of clauses ending in `RETURN`
/// (or a standalone procedure `CALL`).
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub clauses: Vec<Clause>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match(MatchClause),
    Unwind(UnwindClause),
    With(Projection),
    Return(Projection),
    Call(CallClause),
}

impl Clause {
    pub fn span(&self) -> Span {
        match self {
            Clause::Match(c) => c.span,
            Clause::Unwind(c) => c.span,
            Clause::With(p) | Clause::Return(p) => p.span,
            Clause::Call(c) => c.span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    pub optional: bool,
    pub patterns: Vec<PathPattern>,
    /// Acetone extension (spec §5.2): `AT <refspec>` addressing a commit
    /// other than the checked-out one. The refspec is a string literal or
    /// a parameter reference.
    pub at_ref: Option<AtRef>,
    pub where_clause: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AtRef {
    Refspec { value: String, span: Span },
    Parameter { name: String, span: Span },
}

impl AtRef {
    pub fn span(&self) -> Span {
        match self {
            AtRef::Refspec { span, .. } | AtRef::Parameter { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    pub expr: Expr,
    pub alias: String,
    pub span: Span,
}

/// The shared body of `WITH` and `RETURN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub distinct: bool,
    pub items: Vec<ProjectionItem>,
    pub order_by: Vec<SortItem>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
    /// Only meaningful on `WITH`.
    pub where_clause: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionItem {
    /// `RETURN *`
    Star { span: Span },
    Expr {
        expr: Expr,
        alias: Option<String>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortItem {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallClause {
    /// Dotted procedure name, e.g. `["acetone", "diff"]`.
    pub procedure: Vec<String>,
    pub args: Vec<Expr>,
    pub yield_items: Vec<String>,
    pub where_clause: Option<Expr>,
    pub span: Span,
}

// --- Patterns --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// `p = (a)-[]->(b)` path binding.
    pub variable: Option<String>,
    pub start: NodePattern,
    pub steps: Vec<(RelPattern, NodePattern)>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    /// Property map: a `MapLiteral` or a `Parameter` expression.
    pub properties: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub variable: Option<String>,
    /// Alternatives: `[:RUNS|HOSTS]`.
    pub types: Vec<String>,
    pub direction: Direction,
    pub var_length: Option<VarLength>,
    pub properties: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Undirected,
}

/// `*`, `*n`, `*n..m`, `*n..`, `*..m`. An exact length `*n` is
/// represented as `min == max == Some(n)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VarLength {
    pub min: Option<u64>,
    pub max: Option<u64>,
}

// --- Expressions -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal {
        value: Literal,
        span: Span,
    },
    Parameter {
        name: String,
        span: Span,
    },
    Variable {
        name: String,
        span: Span,
    },
    Property {
        base: Box<Expr>,
        key: String,
        span: Span,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    IsNull {
        operand: Box<Expr>,
        negated: bool,
        span: Span,
    },
    /// Function or aggregate invocation; the binder distinguishes them.
    /// `star` is the `count(*)` form.
    FunctionCall {
        name: Vec<String>,
        distinct: bool,
        args: Vec<Expr>,
        star: bool,
        span: Span,
    },
    Case {
        /// `CASE <expr> WHEN ...` operand form; `None` is the searched form.
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_expr: Option<Box<Expr>>,
        span: Span,
    },
    ListLiteral {
        items: Vec<Expr>,
        span: Span,
    },
    ListComprehension {
        variable: String,
        list: Box<Expr>,
        where_clause: Option<Box<Expr>>,
        map: Option<Box<Expr>>,
        span: Span,
    },
    MapLiteral {
        entries: Vec<(String, Expr)>,
        span: Span,
    },
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    Slice {
        base: Box<Expr>,
        from: Option<Box<Expr>>,
        to: Option<Box<Expr>>,
        span: Span,
    },
    /// A relationship pattern used as a boolean predicate in `WHERE`.
    PatternPredicate {
        pattern: Box<PathPattern>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Literal { span, .. }
            | Expr::Parameter { span, .. }
            | Expr::Variable { span, .. }
            | Expr::Property { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::IsNull { span, .. }
            | Expr::FunctionCall { span, .. }
            | Expr::Case { span, .. }
            | Expr::ListLiteral { span, .. }
            | Expr::ListComprehension { span, .. }
            | Expr::MapLiteral { span, .. }
            | Expr::Index { span, .. }
            | Expr::Slice { span, .. }
            | Expr::PatternPredicate { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Minus,
    Plus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    Xor,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    In,
    StartsWith,
    EndsWith,
    Contains,
    RegexMatch,
}
