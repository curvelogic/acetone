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
    /// Level W (Phase 3): `CREATE` of one or more path patterns.
    Create(CreateClause),
    /// Level W: `SET` property/label assignments.
    Set(SetClause),
    /// Level W: `REMOVE` property/label removals.
    Remove(RemoveClause),
    /// Level W: `DELETE` / `DETACH DELETE`.
    Delete(DeleteClause),
    /// Level W: `MERGE` (match-or-create) with `ON CREATE SET`/`ON MATCH SET`.
    Merge(MergeClause),
}

impl Clause {
    pub fn span(&self) -> Span {
        match self {
            Clause::Match(c) => c.span,
            Clause::Unwind(c) => c.span,
            Clause::With(p) | Clause::Return(p) => p.span,
            Clause::Call(c) => c.span,
            Clause::Create(c) => c.span,
            Clause::Set(c) => c.span,
            Clause::Remove(c) => c.span,
            Clause::Delete(c) => c.span,
            Clause::Merge(c) => c.span,
        }
    }

    /// Whether this clause writes to the graph (Level W). A query may end
    /// on a write clause with no trailing `RETURN`.
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Clause::Create(_)
                | Clause::Set(_)
                | Clause::Remove(_)
                | Clause::Delete(_)
                | Clause::Merge(_)
        )
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

/// `CREATE (a:Label {..})-[:TYPE]->(b)`: one or more path patterns whose
/// unbound node/relationship variables are created (spec §5.1 Level W).
#[derive(Debug, Clone, PartialEq)]
pub struct CreateClause {
    pub patterns: Vec<PathPattern>,
    pub span: Span,
}

/// `SET` (spec §5.1 Level W): one or more assignments, comma-separated.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    pub items: Vec<SetItem>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `x.key = value` (a `null` value removes the property).
    Property {
        var: String,
        key: String,
        value: Expr,
        span: Span,
    },
    /// `x = {..}` — replace the whole property map.
    Replace {
        var: String,
        value: Expr,
        span: Span,
    },
    /// `x += {..}` — merge into the property map.
    Merge {
        var: String,
        value: Expr,
        span: Span,
    },
    /// `x:A:B` — add labels (nodes only).
    AddLabels {
        var: String,
        labels: Vec<String>,
        span: Span,
    },
}

impl SetItem {
    pub fn span(&self) -> Span {
        match self {
            SetItem::Property { span, .. }
            | SetItem::Replace { span, .. }
            | SetItem::Merge { span, .. }
            | SetItem::AddLabels { span, .. } => *span,
        }
    }
}

/// `MERGE` (spec §5.1 Level W): match the pattern, or create it whole if it
/// does not exist. `ON CREATE SET`/`ON MATCH SET` apply conditionally.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClause {
    pub pattern: PathPattern,
    pub on_create: Vec<SetItem>,
    pub on_match: Vec<SetItem>,
    pub span: Span,
}

/// `DELETE` / `DETACH DELETE` (spec §5.1 Level W): delete the entities the
/// listed expressions evaluate to. `DETACH` first removes a node's incident
/// relationships; plain `DELETE` of a connected node is an error.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteClause {
    pub detach: bool,
    pub targets: Vec<Expr>,
    pub span: Span,
}

/// `REMOVE` (spec §5.1 Level W): property or label removals.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveClause {
    pub items: Vec<RemoveItem>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    /// `REMOVE x.key`.
    Property {
        var: String,
        key: String,
        span: Span,
    },
    /// `REMOVE x:A:B` — remove labels (nodes only).
    Labels {
        var: String,
        labels: Vec<String>,
        span: Span,
    },
}

impl RemoveItem {
    pub fn span(&self) -> Span {
        match self {
            RemoveItem::Property { span, .. } | RemoveItem::Labels { span, .. } => *span,
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
    /// A list-predicate quantifier: `all|any|none|single (x IN list
    /// WHERE predicate)`.
    Quantifier {
        kind: QuantifierKind,
        variable: String,
        list: Box<Expr>,
        predicate: Box<Expr>,
        span: Span,
    },
    /// `reduce(acc = init, x IN list | expr)`.
    Reduce {
        accumulator: String,
        init: Box<Expr>,
        variable: String,
        list: Box<Expr>,
        expr: Box<Expr>,
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
            | Expr::Quantifier { span, .. }
            | Expr::Reduce { span, .. }
            | Expr::PatternPredicate { span, .. } => *span,
        }
    }

    /// Overwrite this node's span (parenthesised expressions cover their
    /// parentheses for faithful source rendering).
    pub fn set_span(&mut self, new_span: Span) {
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
            | Expr::Quantifier { span, .. }
            | Expr::Reduce { span, .. }
            | Expr::PatternPredicate { span, .. } => *span = new_span,
        }
    }

    /// Borrow every direct child expression, including those buried in an
    /// embedded pattern's property maps. Keep in step with
    /// [`Expr::take_children`].
    fn children(&self) -> Vec<&Expr> {
        let mut out = Vec::new();
        match self {
            Expr::Literal { .. } | Expr::Parameter { .. } | Expr::Variable { .. } => {}
            Expr::Property { base, .. } => out.push(&**base),
            Expr::Unary { operand, .. } => out.push(&**operand),
            Expr::Binary { lhs, rhs, .. } => {
                out.push(&**lhs);
                out.push(&**rhs);
            }
            Expr::IsNull { operand, .. } => out.push(&**operand),
            Expr::FunctionCall { args, .. } => out.extend(args.iter()),
            Expr::Case {
                operand,
                whens,
                else_expr,
                ..
            } => {
                out.extend(operand.iter().map(|b| &**b));
                for (condition, value) in whens {
                    out.push(condition);
                    out.push(value);
                }
                out.extend(else_expr.iter().map(|b| &**b));
            }
            Expr::ListLiteral { items, .. } => out.extend(items.iter()),
            Expr::ListComprehension {
                list,
                where_clause,
                map,
                ..
            } => {
                out.push(&**list);
                out.extend(where_clause.iter().map(|b| &**b));
                out.extend(map.iter().map(|b| &**b));
            }
            Expr::Quantifier {
                list, predicate, ..
            } => {
                out.push(&**list);
                out.push(&**predicate);
            }
            Expr::Reduce {
                init, list, expr, ..
            } => {
                out.push(&**init);
                out.push(&**list);
                out.push(&**expr);
            }
            Expr::MapLiteral { entries, .. } => out.extend(entries.iter().map(|(_, v)| v)),
            Expr::Index { base, index, .. } => {
                out.push(&**base);
                out.push(&**index);
            }
            Expr::Slice { base, from, to, .. } => {
                out.push(&**base);
                out.extend(from.iter().map(|b| &**b));
                out.extend(to.iter().map(|b| &**b));
            }
            Expr::PatternPredicate { pattern, .. } => {
                out.extend(pattern.start.properties.iter());
                for (rel, node) in &pattern.steps {
                    out.extend(rel.properties.iter());
                    out.extend(node.properties.iter());
                }
            }
        }
        out
    }

    /// Move every direct child expression out of `self`, leaving cheap
    /// placeholders behind. Used by the iterative `Drop`. Keep in step
    /// with [`Expr::children`].
    fn take_children(&mut self, out: &mut Vec<Expr>) {
        fn take_box(boxed: &mut Expr, out: &mut Vec<Expr>) {
            let placeholder = Expr::Literal {
                value: Literal::Null,
                span: Span::default(),
            };
            out.push(std::mem::replace(boxed, placeholder));
        }
        match self {
            Expr::Literal { .. } | Expr::Parameter { .. } | Expr::Variable { .. } => {}
            Expr::Property { base, .. } => take_box(base, out),
            Expr::Unary { operand, .. } => take_box(operand, out),
            Expr::Binary { lhs, rhs, .. } => {
                take_box(lhs, out);
                take_box(rhs, out);
            }
            Expr::IsNull { operand, .. } => take_box(operand, out),
            Expr::FunctionCall { args, .. } => out.append(args),
            Expr::Case {
                operand,
                whens,
                else_expr,
                ..
            } => {
                if let Some(boxed) = operand {
                    take_box(boxed, out);
                }
                for (condition, value) in whens.drain(..) {
                    out.push(condition);
                    out.push(value);
                }
                if let Some(boxed) = else_expr {
                    take_box(boxed, out);
                }
            }
            Expr::ListLiteral { items, .. } => out.append(items),
            Expr::ListComprehension {
                list,
                where_clause,
                map,
                ..
            } => {
                take_box(list, out);
                if let Some(boxed) = where_clause {
                    take_box(boxed, out);
                }
                if let Some(boxed) = map {
                    take_box(boxed, out);
                }
            }
            Expr::Quantifier {
                list, predicate, ..
            } => {
                take_box(list, out);
                take_box(predicate, out);
            }
            Expr::Reduce {
                init, list, expr, ..
            } => {
                take_box(init, out);
                take_box(list, out);
                take_box(expr, out);
            }
            Expr::MapLiteral { entries, .. } => {
                out.extend(entries.drain(..).map(|(_, v)| v));
            }
            Expr::Index { base, index, .. } => {
                take_box(base, out);
                take_box(index, out);
            }
            Expr::Slice { base, from, to, .. } => {
                take_box(base, out);
                if let Some(boxed) = from {
                    take_box(boxed, out);
                }
                if let Some(boxed) = to {
                    take_box(boxed, out);
                }
            }
            Expr::PatternPredicate { pattern, .. } => {
                out.extend(pattern.start.properties.take());
                for (rel, node) in &mut pattern.steps {
                    out.extend(rel.properties.take());
                    out.extend(node.properties.take());
                }
            }
        }
    }
}

/// `Expr` trees can be arbitrarily deep in the operator-chain dimension
/// during parsing (the post-parse depth check rejects them, but they must
/// still be torn down). The derived recursive drop glue would overflow the
/// stack on such transients, so drop iteratively via a worklist.
impl Drop for Expr {
    fn drop(&mut self) {
        let mut stack = Vec::new();
        self.take_children(&mut stack);
        while let Some(mut expr) = stack.pop() {
            expr.take_children(&mut stack);
            // `expr` now has no children; its own drop at the end of this
            // iteration finds nothing to recurse into.
        }
    }
}

impl Query {
    /// Maximum expression nesting depth anywhere in the query, computed
    /// iteratively (no recursion, whatever the shape). The parser rejects
    /// queries deeper than its `MAX_AST_DEPTH`, so consumers (binder,
    /// planner, Drop) may rely on the bound.
    pub fn depth(&self) -> usize {
        let mut roots: Vec<&Expr> = Vec::new();
        for clause in &self.clauses {
            match clause {
                Clause::Match(m) => {
                    for pattern in &m.patterns {
                        roots.extend(pattern.start.properties.iter());
                        for (rel, node) in &pattern.steps {
                            roots.extend(rel.properties.iter());
                            roots.extend(node.properties.iter());
                        }
                    }
                    roots.extend(m.where_clause.iter());
                }
                Clause::Create(c) => {
                    for pattern in &c.patterns {
                        roots.extend(pattern.start.properties.iter());
                        for (rel, node) in &pattern.steps {
                            roots.extend(rel.properties.iter());
                            roots.extend(node.properties.iter());
                        }
                    }
                }
                Clause::Set(c) => {
                    for item in &c.items {
                        match item {
                            SetItem::Property { value, .. }
                            | SetItem::Replace { value, .. }
                            | SetItem::Merge { value, .. } => roots.push(value),
                            SetItem::AddLabels { .. } => {}
                        }
                    }
                }
                Clause::Remove(_) => {}
                Clause::Delete(c) => roots.extend(c.targets.iter()),
                Clause::Merge(c) => {
                    roots.extend(c.pattern.start.properties.iter());
                    for (rel, node) in &c.pattern.steps {
                        roots.extend(rel.properties.iter());
                        roots.extend(node.properties.iter());
                    }
                    for item in c.on_create.iter().chain(&c.on_match) {
                        match item {
                            SetItem::Property { value, .. }
                            | SetItem::Replace { value, .. }
                            | SetItem::Merge { value, .. } => roots.push(value),
                            SetItem::AddLabels { .. } => {}
                        }
                    }
                }
                Clause::Unwind(u) => roots.push(&u.expr),
                Clause::With(p) | Clause::Return(p) => {
                    for item in &p.items {
                        if let ProjectionItem::Expr { expr, .. } = item {
                            roots.push(expr);
                        }
                    }
                    roots.extend(p.order_by.iter().map(|s| &s.expr));
                    roots.extend(p.skip.iter());
                    roots.extend(p.limit.iter());
                    roots.extend(p.where_clause.iter());
                }
                Clause::Call(c) => {
                    roots.extend(c.args.iter());
                    roots.extend(c.where_clause.iter());
                }
            }
        }
        let mut max = 0usize;
        let mut stack: Vec<(&Expr, usize)> = roots.into_iter().map(|e| (e, 1)).collect();
        while let Some((expr, depth)) = stack.pop() {
            max = max.max(depth);
            for child in expr.children() {
                stack.push((child, depth + 1));
            }
        }
        max
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
pub enum QuantifierKind {
    All,
    Any,
    None,
    Single,
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
