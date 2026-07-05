//! The bound IR: the AST with names resolved to variable ids, functions
//! resolved against the registry, and planner-facing annotations (index
//! hints, grouping keys). The planner (yzc.5) consumes this; it never
//! sees raw names.

use crate::ast::{BinaryOp, Direction, Literal, UnaryOp, VarLength};
use crate::span::Span;

/// A resolved variable. Ids are dense and stable within one BoundQuery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VarId(pub u32);

/// What a variable denotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Node,
    Relationship,
    /// A var-length relationship variable: a list of relationships.
    RelationshipList,
    Path,
    /// Any computed value (UNWIND element, projection alias).
    Value,
}

impl EntityKind {
    pub fn describe(self) -> &'static str {
        match self {
            EntityKind::Node => "node",
            EntityKind::Relationship => "relationship",
            EntityKind::RelationshipList => "relationship list",
            EntityKind::Path => "path",
            EntityKind::Value => "value",
        }
    }
}

/// One variable's binding record.
#[derive(Debug, Clone)]
pub struct VarBinding {
    pub id: VarId,
    pub name: String,
    pub kind: EntityKind,
    /// Labels known for a node variable from the pattern that bound it
    /// (planner: scan pruning).
    pub labels: Vec<String>,
}

#[derive(Debug)]
pub struct BoundQuery {
    pub clauses: Vec<BoundClause>,
    /// All bindings, indexed by VarId.
    pub variables: Vec<VarBinding>,
}

#[derive(Debug)]
pub enum BoundClause {
    Match {
        optional: bool,
        patterns: Vec<BoundPathPattern>,
        at_ref: Option<crate::ast::AtRef>,
        where_clause: Option<BoundExpr>,
        span: Span,
    },
    Unwind {
        expr: BoundExpr,
        alias: VarId,
        span: Span,
    },
    With(BoundProjection),
    Return(BoundProjection),
    Call {
        /// Index into the procedure registry.
        procedure: &'static ProcedureDef,
        args: Vec<BoundExpr>,
        /// Yielded columns bound as fresh Value variables.
        yields: Vec<(String, VarId)>,
        where_clause: Option<BoundExpr>,
        span: Span,
    },
}

#[derive(Debug)]
pub struct BoundProjection {
    pub distinct: bool,
    pub items: Vec<BoundProjectionItem>,
    pub order_by: Vec<(BoundExpr, bool)>,
    pub skip: Option<BoundExpr>,
    pub limit: Option<BoundExpr>,
    pub where_clause: Option<BoundExpr>,
    /// Item indices that contain no aggregate — the grouping keys when
    /// any item aggregates (planner: Aggregate operator input).
    pub grouping_items: Vec<usize>,
    /// Whether any item aggregates.
    pub aggregating: bool,
    pub span: Span,
}

#[derive(Debug)]
pub struct BoundProjectionItem {
    pub expr: BoundExpr,
    /// The output column name (alias, or the rendered expression text).
    pub name: String,
    /// The variable this item binds in the next scope.
    pub var: VarId,
    pub span: Span,
}

#[derive(Debug)]
pub struct BoundPathPattern {
    pub path_var: Option<VarId>,
    pub start: BoundNodePattern,
    pub steps: Vec<(BoundRelPattern, BoundNodePattern)>,
    pub span: Span,
}

#[derive(Debug)]
pub struct BoundNodePattern {
    pub var: Option<VarId>,
    pub labels: Vec<String>,
    pub properties: Option<BoundExpr>,
    /// Planner hint: an equality the primary key map can seek on
    /// (`label` key-prefix property equated to a literal/parameter in the
    /// pattern's property map), or a declared secondary index.
    pub index_hint: Option<IndexHint>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IndexHint {
    /// The pattern pins the leading key property of `label`.
    KeySeek { label: String },
    /// A declared index `name` on `(label, property)` covers an equality.
    IndexSeek {
        name: String,
        label: String,
        property: String,
    },
}

#[derive(Debug)]
pub struct BoundRelPattern {
    pub var: Option<VarId>,
    pub types: Vec<String>,
    pub direction: Direction,
    pub var_length: Option<VarLength>,
    pub properties: Option<BoundExpr>,
    pub span: Span,
}

#[derive(Debug)]
pub enum BoundExpr {
    Literal {
        value: Literal,
        span: Span,
    },
    Parameter {
        name: String,
        span: Span,
    },
    Variable {
        id: VarId,
        span: Span,
    },
    Property {
        base: Box<BoundExpr>,
        key: String,
        span: Span,
    },
    Unary {
        op: UnaryOp,
        operand: Box<BoundExpr>,
        span: Span,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<BoundExpr>,
        rhs: Box<BoundExpr>,
        span: Span,
    },
    IsNull {
        operand: Box<BoundExpr>,
        negated: bool,
        span: Span,
    },
    Function {
        def: &'static FunctionDef,
        args: Vec<BoundExpr>,
        span: Span,
    },
    Aggregate {
        def: &'static AggregateDef,
        distinct: bool,
        arg: Option<Box<BoundExpr>>,
        span: Span,
    },
    Case {
        operand: Option<Box<BoundExpr>>,
        whens: Vec<(BoundExpr, BoundExpr)>,
        else_expr: Option<Box<BoundExpr>>,
        span: Span,
    },
    ListLiteral {
        items: Vec<BoundExpr>,
        span: Span,
    },
    ListComprehension {
        variable: VarId,
        list: Box<BoundExpr>,
        where_clause: Option<Box<BoundExpr>>,
        map: Option<Box<BoundExpr>>,
        span: Span,
    },
    Quantifier {
        kind: crate::ast::QuantifierKind,
        variable: VarId,
        list: Box<BoundExpr>,
        predicate: Box<BoundExpr>,
        span: Span,
    },
    Reduce {
        accumulator: VarId,
        init: Box<BoundExpr>,
        variable: VarId,
        list: Box<BoundExpr>,
        expr: Box<BoundExpr>,
        span: Span,
    },
    MapLiteral {
        entries: Vec<(String, BoundExpr)>,
        span: Span,
    },
    Index {
        base: Box<BoundExpr>,
        index: Box<BoundExpr>,
        span: Span,
    },
    Slice {
        base: Box<BoundExpr>,
        from: Option<Box<BoundExpr>>,
        to: Option<Box<BoundExpr>>,
        span: Span,
    },
    PatternPredicate {
        pattern: Box<BoundPathPattern>,
        span: Span,
    },
}

// --- Registries -------------------------------------------------------------

/// A scalar/list/string function in the read subset (spec §5.1). Arity is
/// `min..=max` arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct FunctionDef {
    pub name: &'static str,
    pub min_args: usize,
    pub max_args: usize,
}

/// An aggregate function (spec §5.1). All take exactly one argument
/// except `count`, which also has the `count(*)` form.
#[derive(Debug, PartialEq, Eq)]
pub struct AggregateDef {
    pub name: &'static str,
}

/// spec §5.1: "list/string functions" and the core expression helpers the
/// TCK read scenarios use. Names are case-insensitive at lookup.
pub const FUNCTIONS: &[FunctionDef] = &[
    FunctionDef {
        name: "abs",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "ceil",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "coalesce",
        min_args: 1,
        max_args: usize::MAX,
    },
    FunctionDef {
        name: "endNode",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "floor",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "head",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "keys",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "labels",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "last",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "left",
        min_args: 2,
        max_args: 2,
    },
    FunctionDef {
        name: "length",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "lTrim",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "nodes",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "properties",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "range",
        min_args: 2,
        max_args: 3,
    },
    FunctionDef {
        name: "relationships",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "replace",
        min_args: 3,
        max_args: 3,
    },
    FunctionDef {
        name: "reverse",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "right",
        min_args: 2,
        max_args: 2,
    },
    FunctionDef {
        name: "round",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "rTrim",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "sign",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "size",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "split",
        min_args: 2,
        max_args: 2,
    },
    FunctionDef {
        name: "sqrt",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "startNode",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "substring",
        min_args: 2,
        max_args: 3,
    },
    FunctionDef {
        name: "tail",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "toBoolean",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "toFloat",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "toInteger",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "toLower",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "toString",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "toUpper",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "trim",
        min_args: 1,
        max_args: 1,
    },
    FunctionDef {
        name: "type",
        min_args: 1,
        max_args: 1,
    },
];

pub const AGGREGATES: &[AggregateDef] = &[
    AggregateDef { name: "avg" },
    AggregateDef { name: "collect" },
    AggregateDef { name: "count" },
    AggregateDef { name: "max" },
    AggregateDef { name: "min" },
    AggregateDef { name: "sum" },
];

/// An acetone procedure (spec §5.2). Yield columns are the procedure's
/// output row shape; they firm up with the executor (yzc.5/7).
#[derive(Debug, PartialEq, Eq)]
pub struct ProcedureDef {
    pub name: &'static str,
    pub min_args: usize,
    pub max_args: usize,
    pub yields: &'static [&'static str],
}

pub const PROCEDURES: &[ProcedureDef] = &[
    ProcedureDef {
        name: "acetone.log",
        min_args: 0,
        max_args: 1,
        yields: &["commit", "subject"],
    },
    ProcedureDef {
        name: "acetone.diff",
        min_args: 2,
        max_args: 2,
        yields: &["kind", "label", "key"],
    },
    ProcedureDef {
        name: "acetone.blame",
        min_args: 2,
        max_args: 2,
        yields: &["label", "key", "commit"],
    },
    ProcedureDef {
        name: "acetone.conflicts",
        min_args: 0,
        max_args: 0,
        yields: &["label", "key"],
    },
];

pub fn lookup_function(name: &str) -> Option<&'static FunctionDef> {
    FUNCTIONS.iter().find(|f| f.name.eq_ignore_ascii_case(name))
}

pub fn lookup_aggregate(name: &str) -> Option<&'static AggregateDef> {
    AGGREGATES
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(name))
}

pub fn lookup_procedure(name: &str) -> Option<&'static ProcedureDef> {
    PROCEDURES
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
}
