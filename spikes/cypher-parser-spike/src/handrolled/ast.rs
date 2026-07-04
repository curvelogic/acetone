//! Spanned AST for the spike's openCypher subset.

/// Byte range into the query text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start,
            end: other.end,
        }
    }
}

#[derive(Debug)]
pub struct Query {
    pub clauses: Vec<Clause>,
    pub span: Span,
}

#[derive(Debug)]
pub enum Clause {
    Match {
        optional: bool,
        patterns: Vec<PathPattern>,
        /// Acetone extension: `AT <ref>` suffixing a MATCH clause group.
        at_ref: Option<(String, Span)>,
        r#where: Option<Expr>,
        span: Span,
    },
    Unwind {
        expr: Expr,
        alias: String,
        span: Span,
    },
    With(Projection),
    Return(Projection),
    Call {
        procedure: Vec<String>,
        args: Vec<Expr>,
        r#yield: Vec<String>,
        r#where: Option<Expr>,
        span: Span,
    },
    Create {
        patterns: Vec<PathPattern>,
        span: Span,
    },
    Merge {
        pattern: PathPattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
        span: Span,
    },
    Set {
        items: Vec<SetItem>,
        span: Span,
    },
    Remove {
        items: Vec<RemoveItem>,
        span: Span,
    },
    Delete {
        detach: bool,
        exprs: Vec<Expr>,
        span: Span,
    },
}

#[derive(Debug)]
pub struct Projection {
    pub distinct: bool,
    pub items: Vec<ProjectionItem>,
    pub order_by: Vec<(Expr, SortOrder)>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
    pub r#where: Option<Expr>, // WITH ... WHERE
    pub span: Span,
}

#[derive(Debug)]
pub enum ProjectionItem {
    Star,
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug)]
pub enum SetItem {
    Property {
        target: Expr,
        value: Expr,
    },
    Label {
        variable: String,
        labels: Vec<String>,
    },
}

#[derive(Debug)]
pub enum RemoveItem {
    Property(Expr),
    Label {
        variable: String,
        labels: Vec<String>,
    },
}

// --- Patterns ------------------------------------------------------------

#[derive(Debug)]
pub struct PathPattern {
    pub variable: Option<String>,
    pub start: NodePattern,
    pub steps: Vec<(RelPattern, NodePattern)>,
    pub span: Span,
}

#[derive(Debug)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    pub properties: Option<Expr>, // map literal
    pub span: Span,
}

#[derive(Debug)]
pub struct RelPattern {
    pub variable: Option<String>,
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

#[derive(Debug, Default)]
pub struct VarLength {
    pub min: Option<u64>,
    pub max: Option<u64>,
}

// --- Expressions ----------------------------------------------------------

#[derive(Debug)]
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
    FunctionCall {
        name: Vec<String>,
        distinct: bool,
        args: Vec<Expr>,
        star: bool,
        span: Span,
    },
    Case {
        /// `CASE <expr> WHEN ...` operand form; None for the searched form.
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        r#else: Option<Box<Expr>>,
        span: Span,
    },
    ListLiteral {
        items: Vec<Expr>,
        span: Span,
    },
    ListComprehension {
        variable: String,
        list: Box<Expr>,
        r#where: Option<Box<Expr>>,
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
    /// Pattern used as a boolean predicate inside WHERE.
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

#[derive(Debug)]
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
