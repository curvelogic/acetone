//! Expression evaluation over the bound IR with openCypher null
//! semantics, TCK-verified. Aggregate sub-expressions are computed by the
//! Aggregate operator; here they read from a pre-computed slot sequence
//! in deterministic traversal order.

use std::cell::Cell;
use std::collections::BTreeMap;

use crate::ast::QuantifierKind;
use crate::ast::{BinaryOp, Literal, UnaryOp};
use crate::bind::bound::{BoundExpr, BoundPathPattern, VarId};
use crate::exec::source::GraphSource;
use crate::exec::value::{MAX_VALUE_DEPTH, Ternary, Value};
use crate::span::Span;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExecError {
    #[error("type error: {message}")]
    Type { message: String, span: Span },

    #[error("integer overflow")]
    Overflow { span: Span },

    #[error("division by zero")]
    DivisionByZero { span: Span },

    #[error("missing parameter '{name}'")]
    MissingParameter { name: String, span: Span },

    #[error("{feature} is not implemented yet")]
    Unsupported { feature: &'static str, span: Span },

    #[error("invalid argument: {message}")]
    InvalidArgument { message: String, span: Span },

    #[error("query exceeded the {limit} resource limit")]
    ResourceExceeded { limit: ResourceLimit, span: Span },
}

/// Which governed resource a query exhausted (see [`crate::exec::governor`]).
/// Carried by [`ExecError::ResourceExceeded`] so a caller can tell *which*
/// cap tripped without string-matching the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLimit {
    /// The canonical deterministic work-unit odometer.
    WorkUnits,
    /// The size of a single materialised result-row set.
    ResultRows,
    /// Cumulative variable-length / expansion hops.
    ExpansionSteps,
    /// The length of a single list/collection.
    CollectionLen,
    /// The optional wall-clock backstop.
    WallClock,
    /// The container nesting depth of a single constructed value
    /// (`MAX_VALUE_DEPTH` in `exec::value` — a fixed structural cap that
    /// keeps recursive value walks stack-safe, not a
    /// [`crate::exec::governor::QueryLimits`] field).
    ValueDepth,
}

impl std::fmt::Display for ResourceLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ResourceLimit::WorkUnits => "work-unit",
            ResourceLimit::ResultRows => "result-row",
            ResourceLimit::ExpansionSteps => "expansion-step",
            ResourceLimit::CollectionLen => "collection-size",
            ResourceLimit::WallClock => "wall-clock",
            ResourceLimit::ValueDepth => "value-nesting-depth",
        };
        f.write_str(name)
    }
}

impl ExecError {
    /// The source span this error points at. Every execution error carries a
    /// span so it can be located in the query the same way parse and bind
    /// errors are.
    pub fn span(&self) -> Span {
        match self {
            ExecError::Type { span, .. }
            | ExecError::Overflow { span }
            | ExecError::DivisionByZero { span }
            | ExecError::MissingParameter { span, .. }
            | ExecError::Unsupported { span, .. }
            | ExecError::InvalidArgument { span, .. }
            | ExecError::ResourceExceeded { span, .. } => *span,
        }
    }

    /// Render the error with 1-based line/column against the source it came
    /// from, matching [`crate::error::ParseError::render`] and
    /// [`crate::bind::BindError::render`] so every layer locates errors the
    /// same way.
    pub fn render(&self, source: &str) -> String {
        let (line, col) = self.span().line_col(source);
        format!("line {line}, column {col}: {self}")
    }
}

/// One result row: values indexed by VarId.
#[derive(Debug, Clone, Default)]
pub struct Row {
    slots: BTreeMap<u32, Value>,
}

impl Row {
    pub fn get(&self, var: VarId) -> Value {
        self.slots.get(&var.0).cloned().unwrap_or(Value::Null)
    }

    /// Distinguishes a variable bound to null (matches nothing in a
    /// pattern) from one not bound at all (a fresh pattern variable).
    pub fn contains(&self, var: VarId) -> bool {
        self.slots.contains_key(&var.0)
    }

    pub fn set(&mut self, var: VarId, value: Value) {
        self.slots.insert(var.0, value);
    }

    /// Apply `f` to every bound value in place — used after a write clause
    /// to re-resolve entity values (nodes/relationships) from the graph
    /// overlay so aliases and downstream clauses observe the update.
    pub fn update_all(&mut self, mut f: impl FnMut(&mut Value)) {
        for value in self.slots.values_mut() {
            f(value);
        }
    }
}

pub struct EvalCtx<'a> {
    pub graph: &'a dyn GraphSource,
    pub parameters: &'a BTreeMap<String, Value>,
    /// The per-query resource budget. Shared by reference (interior-mutable
    /// counters) because `EvalCtx` is rebuilt per clause but the budget spans
    /// the whole query — see [`crate::exec::governor`].
    pub governor: &'a super::governor::Governor,
    /// Pre-computed aggregate results, consumed in traversal order by
    /// `BoundExpr::Aggregate` nodes (set by the Aggregate operator).
    pub aggregates: Option<(&'a [Value], Cell<usize>)>,
}

impl<'a> EvalCtx<'a> {
    pub fn new(
        graph: &'a dyn GraphSource,
        parameters: &'a BTreeMap<String, Value>,
        governor: &'a super::governor::Governor,
    ) -> Self {
        EvalCtx {
            graph,
            parameters,
            governor,
            aggregates: None,
        }
    }
}

pub fn eval(expr: &BoundExpr, row: &Row, ctx: &EvalCtx) -> Result<Value, ExecError> {
    match expr {
        BoundExpr::Literal { value, span: _ } => Ok(match value {
            Literal::Null => Value::Null,
            Literal::Boolean(b) => Value::Bool(*b),
            Literal::Integer(n) => Value::Int(*n),
            Literal::Float(x) => Value::Float(*x),
            Literal::String(s) => Value::String(s.clone()),
        }),
        BoundExpr::Parameter { name, span } => {
            ctx.parameters
                .get(name)
                .cloned()
                .ok_or_else(|| ExecError::MissingParameter {
                    name: name.clone(),
                    span: *span,
                })
        }
        BoundExpr::Variable { id, .. } => Ok(row.get(*id)),
        BoundExpr::Property { base, key, span } => {
            let base = eval(base, row, ctx)?;
            property_access(&base, key, *span)
        }
        BoundExpr::Unary { op, operand, span } => {
            // A read carrier decays to its string rendering the moment an
            // operator consumes it (ADR-0038): from here on it is a string.
            let value = eval(operand, row, ctx)?.decayed();
            unary(*op, value, *span)
        }
        BoundExpr::Binary { op, lhs, rhs, span } => {
            // AND/OR/XOR need ternary short-circuit handling over both
            // sides; everything else evaluates strictly.
            match op {
                BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                    let a = truth(&eval(lhs, row, ctx)?, *span)?;
                    let b = truth(&eval(rhs, row, ctx)?, *span)?;
                    Ok(ternary_to_value(logic(*op, a, b)))
                }
                _ => {
                    // Both operands decay a read carrier to its string
                    // rendering before any comparison, arithmetic, `+`, `IN`
                    // or `STARTS WITH`/`CONTAINS` (ADR-0038).
                    let a = eval(lhs, row, ctx)?.decayed();
                    let b = eval(rhs, row, ctx)?.decayed();
                    binary(*op, a, b, *span, ctx.governor)
                }
            }
        }
        BoundExpr::IsNull {
            operand,
            negated,
            span: _,
        } => {
            let value = eval(operand, row, ctx)?;
            Ok(Value::Bool(value.is_null() != *negated))
        }
        BoundExpr::Function { def, args, span } => {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                // A carrier passed to a function decays to its string rendering
                // (ADR-0038), so `toUpper`/`substring`/`toString`/… see the same
                // string the runtime saw before the carrier existed.
                values.push(eval(arg, row, ctx)?.decayed());
            }
            crate::exec::functions::call(def.name, values, *span, ctx.graph, ctx.governor)
        }
        BoundExpr::Aggregate { span, .. } => {
            let Some((values, cursor)) = &ctx.aggregates else {
                return Err(ExecError::Unsupported {
                    feature: "aggregate outside projection",
                    span: *span,
                });
            };
            let index = cursor.get();
            cursor.set(index + 1);
            values.get(index).cloned().ok_or(ExecError::Unsupported {
                feature: "aggregate slot mismatch",
                span: *span,
            })
        }
        BoundExpr::Case {
            operand,
            whens,
            else_expr,
            span: _,
        } => {
            match operand {
                Some(operand) => {
                    let subject = eval(operand, row, ctx)?;
                    for (candidate, result) in whens {
                        let candidate = eval(candidate, row, ctx)?;
                        if subject.eq3(&candidate) == Some(true) {
                            return eval(result, row, ctx);
                        }
                    }
                }
                None => {
                    for (condition, result) in whens {
                        let condition = eval(condition, row, ctx)?;
                        if truth(&condition, result.span())? == Some(true) {
                            return eval(result, row, ctx);
                        }
                    }
                }
            }
            match else_expr {
                Some(expr) => eval(expr, row, ctx),
                None => Ok(Value::Null),
            }
        }
        BoundExpr::ListLiteral { items, span } => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                let value = eval(item, row, ctx)?;
                // The construction-time depth cap (acetone-19x): a list
                // literal inside a reduce is exactly how a shallow AST builds
                // an arbitrarily deep value.
                ensure_nestable(&value, *span)?;
                values.push(value);
            }
            Ok(Value::List(values))
        }
        BoundExpr::ListComprehension {
            variable,
            list,
            where_clause,
            map,
            span,
        } => {
            let list = eval(list, row, ctx)?;
            let items = match list {
                Value::Null => return Ok(Value::Null),
                Value::List(items) => items,
                other => {
                    return Err(ExecError::Type {
                        message: format!("expected a list, got {}", other.type_name()),
                        span: *span,
                    });
                }
            };
            let mut out = Vec::new();
            let mut inner = row.clone();
            for item in items {
                inner.set(*variable, item.clone());
                if let Some(predicate) = where_clause
                    && truth(&eval(predicate, &inner, ctx)?, *span)? != Some(true)
                {
                    continue;
                }
                // Charge each produced element (linear work) against the
                // collection cap so a comprehension over a large source list is
                // bounded without over-charging quadratically.
                ctx.governor.collection_push(out.len())?;
                let value = match map {
                    Some(map) => eval(map, &inner, ctx)?,
                    None => item,
                };
                // Construction-time depth cap (acetone-19x).
                ensure_nestable(&value, *span)?;
                out.push(value);
            }
            Ok(Value::List(out))
        }
        BoundExpr::Quantifier {
            kind,
            variable,
            list,
            predicate,
            span,
        } => {
            let list = eval(list, row, ctx)?;
            let items = match list {
                Value::Null => return Ok(Value::Null),
                Value::List(items) => items,
                other => {
                    return Err(ExecError::Type {
                        message: format!("expected a list, got {}", other.type_name()),
                        span: *span,
                    });
                }
            };
            quantify(*kind, *variable, &items, predicate, row, ctx, *span)
        }
        BoundExpr::Reduce {
            accumulator,
            init,
            variable,
            list,
            expr,
            span,
        } => {
            let list = eval(list, row, ctx)?;
            let items = match list {
                Value::Null => return Ok(Value::Null),
                Value::List(items) => items,
                other => {
                    return Err(ExecError::Type {
                        message: format!("reduce expects a list, got {}", other.type_name()),
                        span: *span,
                    });
                }
            };
            let mut acc = eval(init, row, ctx)?;
            let mut inner = row.clone();
            for (index, item) in items.into_iter().enumerate() {
                // Charge each fold step so a reduce over a large list is
                // CPU-accounted, not just bounded by the source list's size.
                ctx.governor.collection_push(index)?;
                inner.set(*accumulator, acc);
                inner.set(*variable, item);
                acc = eval(expr, &inner, ctx)?;
            }
            Ok(acc)
        }
        BoundExpr::MapLiteral { entries, span } => {
            let mut map = BTreeMap::new();
            for (key, value) in entries {
                let value = eval(value, row, ctx)?;
                // Construction-time depth cap (acetone-19x).
                ensure_nestable(&value, *span)?;
                map.insert(key.clone(), value);
            }
            Ok(Value::Map(map))
        }
        BoundExpr::Index { base, index, span } => {
            // Decay a carrier used as a container or a map/list key (ADR-0038);
            // a carrier is a scalar, so as a base it errors exactly as its
            // string rendering would, and as a key it matches by that string.
            let base = eval(base, row, ctx)?.decayed();
            let index = eval(index, row, ctx)?.decayed();
            index_access(base, index, *span)
        }
        BoundExpr::Slice {
            base,
            from,
            to,
            span,
        } => {
            // A carrier decays before slicing (ADR-0038): as a scalar base it
            // errors as its string rendering would; the bounds are integers.
            let base = eval(base, row, ctx)?.decayed();
            let from = match from {
                Some(expr) => Some(eval(expr, row, ctx)?.decayed()),
                None => None,
            };
            let to = match to {
                Some(expr) => Some(eval(expr, row, ctx)?.decayed()),
                None => None,
            };
            slice_access(base, from, to, *span)
        }
        BoundExpr::PatternPredicate { pattern, span: _ } => {
            Ok(Value::Bool(pattern_exists(pattern, row, ctx)?))
        }
    }
}

/// Refuse to nest `value` under one more container level once its own
/// container nesting has reached [`MAX_VALUE_DEPTH`] (acetone-19x). Called at
/// every construction seam that can deepen a runtime value — list/map
/// literals, comprehension elements, the list-push arms of `+`, and
/// `collect()` — so no constructible value ever exceeds the cap, and the
/// recursive walks over values (`distinct_key`, `global_cmp`, `eq3`,
/// `format`, clone, drop glue, persist) are bounded on a default stack.
/// Seams that cannot deepen a value need no check: list concatenation and
/// slicing reuse already-capped children, and the flat builders
/// (`range`/`split`/`keys`/`labels`) produce scalars.
pub(crate) fn ensure_nestable(value: &Value, span: Span) -> Result<(), ExecError> {
    if value.nesting_exceeds(MAX_VALUE_DEPTH - 1) {
        return Err(ExecError::ResourceExceeded {
            limit: ResourceLimit::ValueDepth,
            span,
        });
    }
    Ok(())
}

/// openCypher truth: booleans as themselves, null as unknown, anything
/// else a type error.
pub fn truth(value: &Value, span: Span) -> Result<Ternary, ExecError> {
    match value {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(ExecError::Type {
            message: format!("expected a boolean, got {}", other.type_name()),
            span,
        }),
    }
}

fn ternary_to_value(t: Ternary) -> Value {
    match t {
        Some(b) => Value::Bool(b),
        None => Value::Null,
    }
}

fn logic(op: BinaryOp, a: Ternary, b: Ternary) -> Ternary {
    match op {
        BinaryOp::And => match (a, b) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        BinaryOp::Or => match (a, b) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        BinaryOp::Xor => match (a, b) {
            (Some(a), Some(b)) => Some(a != b),
            _ => None,
        },
        _ => unreachable!("logic() called for non-logical op"),
    }
}

fn unary(op: UnaryOp, value: Value, span: Span) -> Result<Value, ExecError> {
    match op {
        UnaryOp::Not => Ok(ternary_to_value(truth(&value, span)?.map(|b| !b))),
        UnaryOp::Minus => match value {
            Value::Null => Ok(Value::Null),
            Value::Int(n) => n
                .checked_neg()
                .map(Value::Int)
                .ok_or(ExecError::Overflow { span }),
            Value::Float(x) => Ok(Value::Float(-x)),
            other => Err(ExecError::Type {
                message: format!("cannot negate {}", other.type_name()),
                span,
            }),
        },
        UnaryOp::Plus => match value {
            Value::Null | Value::Int(_) | Value::Float(_) => Ok(value),
            other => Err(ExecError::Type {
                message: format!("cannot apply unary + to {}", other.type_name()),
                span,
            }),
        },
    }
}

fn binary(
    op: BinaryOp,
    a: Value,
    b: Value,
    span: Span,
    governor: &super::governor::Governor,
) -> Result<Value, ExecError> {
    use BinaryOp::*;
    match op {
        Eq => Ok(ternary_to_value(a.eq3(&b))),
        Ne => Ok(ternary_to_value(a.eq3(&b).map(|x| !x))),
        // NaN compared with any number is false, not null (TCK
        // Comparison2 [5]); cross-type and null comparisons stay null.
        Lt | Le | Gt | Ge if nan_comparison(&a, &b) => Ok(Value::Bool(false)),
        Lt => Ok(ternary_to_value(
            a.cmp3(&b).map(|o| o == std::cmp::Ordering::Less),
        )),
        Le => Ok(ternary_to_value(
            a.cmp3(&b).map(|o| o != std::cmp::Ordering::Greater),
        )),
        Gt => Ok(ternary_to_value(
            a.cmp3(&b).map(|o| o == std::cmp::Ordering::Greater),
        )),
        Ge => Ok(ternary_to_value(
            a.cmp3(&b).map(|o| o != std::cmp::Ordering::Less),
        )),
        Add => add(a, b, span, governor),
        Sub => arith(a, b, span, i64::checked_sub, |x, y| x - y),
        Mul => arith(a, b, span, i64::checked_mul, |x, y| x * y),
        Div => divide(a, b, span),
        Mod => modulo(a, b, span),
        Pow => power(a, b, span),
        In => list_membership(a, b, span),
        StartsWith => string_pred(a, b, |s, t| s.starts_with(t)),
        EndsWith => string_pred(a, b, |s, t| s.ends_with(t)),
        Contains => string_pred(a, b, |s, t| s.contains(t)),
        RegexMatch => Err(ExecError::Unsupported {
            feature: "regular expressions",
            span,
        }),
        And | Or | Xor => unreachable!("handled in eval"),
    }
}

/// Both operands numeric and at least one NaN.
fn nan_comparison(a: &Value, b: &Value) -> bool {
    let is_number = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_));
    let is_nan = |v: &Value| matches!(v, Value::Float(x) if x.is_nan());
    is_number(a) && is_number(b) && (is_nan(a) || is_nan(b))
}

fn add(
    a: Value,
    b: Value,
    span: Span,
    governor: &super::governor::Governor,
) -> Result<Value, ExecError> {
    use Value::*;
    // `+` is the one operator that materialises an unboundedly larger value
    // than its inputs — repeated `acc + acc` (a doubling `reduce`) or `s + x`
    // over a large list. Charge the *resulting* collection/string size against
    // the governor before allocating it, so those are bounded up front.
    let concat = |governor: &super::governor::Governor, len: usize| governor.collection(len as u64);
    match (a, b) {
        (Null, _) | (_, Null) => Ok(Null),
        (Int(a), Int(b)) => a
            .checked_add(b)
            .map(Int)
            .ok_or(ExecError::Overflow { span }),
        (Int(a), Float(b)) => Ok(Float(a as f64 + b)),
        (Float(a), Int(b)) => Ok(Float(a + b as f64)),
        (Float(a), Float(b)) => Ok(Float(a + b)),
        (String(a), String(b)) => {
            concat(governor, a.len() + b.len())?;
            Ok(String(a + &b))
        }
        // Charge the *exact* rendered length of the number (f64 Display can be
        // up to ~309 bytes), so the collection cap is never overshot.
        (String(a), Int(b)) => {
            let r = b.to_string();
            concat(governor, a.len() + r.len())?;
            Ok(String(a + &r))
        }
        (String(a), Float(b)) => {
            let r = crate::exec::functions::format_float(b);
            concat(governor, a.len() + r.len())?;
            Ok(String(a + &r))
        }
        (Int(a), String(b)) => {
            let r = a.to_string();
            concat(governor, r.len() + b.len())?;
            Ok(String(r + &b))
        }
        (Float(a), String(b)) => {
            let r = crate::exec::functions::format_float(a);
            concat(governor, r.len() + b.len())?;
            Ok(String(r + &b))
        }
        (List(mut a), List(b)) => {
            // Concatenation cannot deepen: the result's children are the
            // operands' already-capped children, so no depth check is needed.
            concat(governor, a.len() + b.len())?;
            a.extend(b);
            Ok(List(a))
        }
        (List(mut a), b) => {
            concat(governor, a.len() + 1)?;
            // The pushed element gains a container level (acetone-19x); the
            // receiving list's own children keep their depth.
            ensure_nestable(&b, span)?;
            a.push(b);
            Ok(List(a))
        }
        (a, List(mut b)) => {
            concat(governor, b.len() + 1)?;
            ensure_nestable(&a, span)?;
            b.insert(0, a);
            Ok(List(b))
        }
        (a, b) => Err(ExecError::Type {
            message: format!("cannot add {} and {}", a.type_name(), b.type_name()),
            span,
        }),
    }
}

fn arith(
    a: Value,
    b: Value,
    span: Span,
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> Result<Value, ExecError> {
    use Value::*;
    match (a, b) {
        (Null, _) | (_, Null) => Ok(Null),
        (Int(a), Int(b)) => int_op(a, b).map(Int).ok_or(ExecError::Overflow { span }),
        (Int(a), Float(b)) => Ok(Float(float_op(a as f64, b))),
        (Float(a), Int(b)) => Ok(Float(float_op(a, b as f64))),
        (Float(a), Float(b)) => Ok(Float(float_op(a, b))),
        (a, b) => Err(ExecError::Type {
            message: format!(
                "arithmetic needs numbers, got {} and {}",
                a.type_name(),
                b.type_name()
            ),
            span,
        }),
    }
}

fn divide(a: Value, b: Value, span: Span) -> Result<Value, ExecError> {
    use Value::*;
    match (&a, &b) {
        (Int(_), Int(0)) => Err(ExecError::DivisionByZero { span }),
        // `i64::MIN / -1` overflows i64 — an error, not a silent wrap.
        (Int(x), Int(y)) => x
            .checked_div(*y)
            .map(Int)
            .ok_or(ExecError::Overflow { span }),
        _ => arith(a, b, span, i64::checked_div, |x, y| x / y),
    }
}

fn modulo(a: Value, b: Value, span: Span) -> Result<Value, ExecError> {
    use Value::*;
    match (&a, &b) {
        (Int(_), Int(0)) => Err(ExecError::DivisionByZero { span }),
        (Int(x), Int(y)) => x
            .checked_rem(*y)
            .map(Int)
            .ok_or(ExecError::Overflow { span }),
        _ => arith(a, b, span, i64::checked_rem, |x, y| x % y),
    }
}

fn power(a: Value, b: Value, span: Span) -> Result<Value, ExecError> {
    use Value::*;
    match (a, b) {
        (Null, _) | (_, Null) => Ok(Null),
        // openCypher exponentiation always yields a float.
        (a @ (Int(_) | Float(_)), b @ (Int(_) | Float(_))) => {
            let (x, y) = (as_f64(&a), as_f64(&b));
            Ok(Float(x.powf(y)))
        }
        (a, b) => Err(ExecError::Type {
            message: format!("cannot raise {} to {}", a.type_name(), b.type_name()),
            span,
        }),
    }
}

fn as_f64(value: &Value) -> f64 {
    match value {
        Value::Int(n) => *n as f64,
        Value::Float(x) => *x,
        _ => unreachable!("checked by caller"),
    }
}

/// `x IN list` with openCypher null semantics: null list -> null; any
/// null-valued comparison without a definite true -> null.
fn list_membership(needle: Value, haystack: Value, span: Span) -> Result<Value, ExecError> {
    match haystack {
        Value::Null => Ok(Value::Null),
        Value::List(items) => {
            let mut saw_null = needle.is_null() && !items.is_empty();
            for item in &items {
                match needle.eq3(item) {
                    Some(true) => return Ok(Value::Bool(true)),
                    None => saw_null = true,
                    Some(false) => {}
                }
            }
            if needle.is_null() && items.is_empty() {
                return Ok(Value::Bool(false));
            }
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Bool(false)
            })
        }
        other => Err(ExecError::Type {
            message: format!("IN needs a list, got {}", other.type_name()),
            span,
        }),
    }
}

fn string_pred(a: Value, b: Value, pred: fn(&str, &str) -> bool) -> Result<Value, ExecError> {
    match (a, b) {
        (Value::String(a), Value::String(b)) => Ok(Value::Bool(pred(&a, &b))),
        // Null or non-string operands yield null, per openCypher.
        _ => Ok(Value::Null),
    }
}

fn property_access(base: &Value, key: &str, span: Span) -> Result<Value, ExecError> {
    match base {
        Value::Null => Ok(Value::Null),
        Value::Node(node) => Ok(node.properties.get(key).cloned().unwrap_or(Value::Null)),
        Value::Relationship(rel) => Ok(rel.properties.get(key).cloned().unwrap_or(Value::Null)),
        Value::Map(map) => Ok(map.get(key).cloned().unwrap_or(Value::Null)),
        other => Err(ExecError::Type {
            message: format!("cannot access property on {}", other.type_name()),
            span,
        }),
    }
}

fn index_access(base: Value, index: Value, span: Span) -> Result<Value, ExecError> {
    match (base, index) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::List(items), Value::Int(i)) => {
            let len = items.len() as i64;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                Ok(Value::Null)
            } else {
                Ok(items[at as usize].clone())
            }
        }
        (Value::Map(map), Value::String(key)) => Ok(map.get(&key).cloned().unwrap_or(Value::Null)),
        (Value::Node(node), Value::String(key)) => {
            Ok(node.properties.get(&key).cloned().unwrap_or(Value::Null))
        }
        (Value::Relationship(rel), Value::String(key)) => {
            Ok(rel.properties.get(&key).cloned().unwrap_or(Value::Null))
        }
        (Value::List(_), other) => Err(ExecError::Type {
            message: format!("list index must be an integer, got {}", other.type_name()),
            span,
        }),
        (base, _) => Err(ExecError::Type {
            message: format!("cannot index {}", base.type_name()),
            span,
        }),
    }
}

fn slice_access(
    base: Value,
    from: Option<Value>,
    to: Option<Value>,
    span: Span,
) -> Result<Value, ExecError> {
    let items = match base {
        Value::Null => return Ok(Value::Null),
        Value::List(items) => items,
        other => {
            return Err(ExecError::Type {
                message: format!("cannot slice {}", other.type_name()),
                span,
            });
        }
    };
    let len = items.len() as i64;
    let resolve = |bound: Option<Value>, default: i64| -> Result<Option<i64>, ExecError> {
        match bound {
            None => Ok(Some(default)),
            Some(Value::Null) => Ok(None),
            Some(Value::Int(i)) => Ok(Some((if i < 0 { i + len } else { i }).clamp(0, len))),
            Some(other) => Err(ExecError::Type {
                message: format!("slice bound must be an integer, got {}", other.type_name()),
                span,
            }),
        }
    };
    let Some(start) = resolve(from, 0)? else {
        return Ok(Value::Null);
    };
    let Some(end) = resolve(to, len)? else {
        return Ok(Value::Null);
    };
    if start >= end {
        return Ok(Value::List(Vec::new()));
    }
    Ok(Value::List(items[start as usize..end as usize].to_vec()))
}

/// Does any assignment of the pattern's unbound (anonymous) parts exist,
/// anchored on the row's bound variables? Pattern predicates cannot
/// introduce variables (binder-enforced), so this is a pure probe.
fn pattern_exists(pattern: &BoundPathPattern, row: &Row, ctx: &EvalCtx) -> Result<bool, ExecError> {
    let starts = match &pattern.start.var {
        Some(var) => match row.get(*var) {
            Value::Node(node) => vec![node],
            Value::Null => return Ok(false),
            _ => return Ok(false),
        },
        None => ctx.graph.nodes_by_labels(&pattern.start.labels),
    };
    for start in starts {
        if !pattern
            .start
            .labels
            .iter()
            .all(|l| start.labels.contains(l))
        {
            continue;
        }
        if probe_steps(&pattern.steps, 0, &start, row, ctx)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn probe_steps(
    steps: &[(
        crate::bind::bound::BoundRelPattern,
        crate::bind::bound::BoundNodePattern,
    )],
    at: usize,
    from: &crate::exec::value::NodeValue,
    row: &Row,
    ctx: &EvalCtx,
) -> Result<bool, ExecError> {
    let Some((rel, node)) = steps.get(at) else {
        return Ok(true);
    };
    if rel.var_length.is_some() {
        return Err(ExecError::Unsupported {
            feature: "var-length pattern predicates",
            span: rel.span,
        });
    }
    for (rel_value, neighbour) in ctx.graph.expand(&from.id, rel.direction, &rel.types) {
        // A bound relationship variable pins the exact relationship.
        if let Some(var) = rel.var {
            match row.get(var) {
                Value::Relationship(bound) if bound.id == rel_value.id => {}
                Value::Relationship(_) => continue,
                _ => continue,
            }
        }
        // A bound node variable pins the neighbour.
        if let Some(var) = node.var {
            match row.get(var) {
                Value::Node(bound) if bound.id == neighbour.id => {}
                _ => continue,
            }
        }
        if !node.labels.iter().all(|l| neighbour.labels.contains(l)) {
            continue;
        }
        if probe_steps(steps, at + 1, &neighbour, row, ctx)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Evaluate a list-predicate quantifier with openCypher three-valued
/// semantics. Counting elements whose predicate is true (T), false (F)
/// or null (N):
/// - any:    T>=1 -> true; T=0,N>=1 -> null; else false
/// - all:    F>=1 -> false; F=0,N>=1 -> null; else true
/// - none:   T>=1 -> false; T=0,N>=1 -> null; else true
/// - single: T=1,N=0 -> true; T>=2 -> false; T=0,N=0 -> false; else null
#[allow(clippy::too_many_arguments)]
fn quantify(
    kind: QuantifierKind,
    variable: VarId,
    items: &[Value],
    predicate: &BoundExpr,
    row: &Row,
    ctx: &EvalCtx,
    span: Span,
) -> Result<Value, ExecError> {
    let mut t = 0usize;
    let mut n = 0usize;
    let mut inner = row.clone();
    for (index, item) in items.iter().enumerate() {
        // Charge each predicate evaluation so a quantifier over a large list
        // is CPU-accounted.
        ctx.governor.collection_push(index)?;
        inner.set(variable, item.clone());
        match truth(&eval(predicate, &inner, ctx)?, span)? {
            Some(true) => t += 1,
            Some(false) => {}
            None => n += 1,
        }
    }
    let f = items.len() - t - n;
    let value = match kind {
        QuantifierKind::Any => {
            if t >= 1 {
                Value::Bool(true)
            } else if n >= 1 {
                Value::Null
            } else {
                Value::Bool(false)
            }
        }
        QuantifierKind::All => {
            if f >= 1 {
                Value::Bool(false)
            } else if n >= 1 {
                Value::Null
            } else {
                Value::Bool(true)
            }
        }
        QuantifierKind::None => {
            if t >= 1 {
                Value::Bool(false)
            } else if n >= 1 {
                Value::Null
            } else {
                Value::Bool(true)
            }
        }
        QuantifierKind::Single => {
            if t == 1 && n == 0 {
                Value::Bool(true)
            } else if t >= 2 || (t == 0 && n == 0) {
                Value::Bool(false)
            } else {
                Value::Null
            }
        }
    };
    Ok(value)
}

impl BoundExpr {
    pub fn span(&self) -> Span {
        match self {
            BoundExpr::Literal { span, .. }
            | BoundExpr::Parameter { span, .. }
            | BoundExpr::Variable { span, .. }
            | BoundExpr::Property { span, .. }
            | BoundExpr::Unary { span, .. }
            | BoundExpr::Binary { span, .. }
            | BoundExpr::IsNull { span, .. }
            | BoundExpr::Function { span, .. }
            | BoundExpr::Aggregate { span, .. }
            | BoundExpr::Case { span, .. }
            | BoundExpr::ListLiteral { span, .. }
            | BoundExpr::ListComprehension { span, .. }
            | BoundExpr::Quantifier { span, .. }
            | BoundExpr::Reduce { span, .. }
            | BoundExpr::MapLiteral { span, .. }
            | BoundExpr::Index { span, .. }
            | BoundExpr::Slice { span, .. }
            | BoundExpr::PatternPredicate { span, .. } => *span,
        }
    }
}
