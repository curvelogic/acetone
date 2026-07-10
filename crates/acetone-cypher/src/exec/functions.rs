//! Implementations of the §5.1 function registry. Arity is validated by
//! the binder; null arguments generally yield null per openCypher.

use crate::exec::eval::ExecError;
use crate::exec::source::GraphSource;
use crate::exec::value::Value;
use crate::span::Span;

/// Upper bound on the number of elements `range()` may materialise, so a query
/// like `range(0, 9223372036854775807)` cannot exhaust memory. A resource
/// governor will make this configurable (acetone-iq6); until then it is a fixed,
/// generous cap.
const MAX_RANGE_ELEMENTS: i128 = 10_000_000;

pub fn call(
    name: &str,
    args: Vec<Value>,
    span: Span,
    graph: &dyn GraphSource,
) -> Result<Value, ExecError> {
    let type_error = |message: String| ExecError::Type { message, span };

    // coalesce is the only function that looks past a null argument.
    if name.eq_ignore_ascii_case("coalesce") {
        return Ok(args
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(Value::Null));
    }
    // range has no null-propagation (its arguments must be integers).
    if !name.eq_ignore_ascii_case("range") && args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }

    let arg = |index: usize| -> Value { args.get(index).cloned().unwrap_or(Value::Null) };

    match () {
        _ if name.eq_ignore_ascii_case("abs") => match arg(0) {
            Value::Int(n) => n
                .checked_abs()
                .map(Value::Int)
                .ok_or(ExecError::Overflow { span }),
            Value::Float(x) => Ok(Value::Float(x.abs())),
            other => Err(type_error(format!(
                "abs() needs a number, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("ceil") => {
            numeric_to_float(arg(0), span).map(|x| Value::Float(x.ceil()))
        }
        _ if name.eq_ignore_ascii_case("floor") => {
            numeric_to_float(arg(0), span).map(|x| Value::Float(x.floor()))
        }
        _ if name.eq_ignore_ascii_case("round") => {
            numeric_to_float(arg(0), span).map(|x| Value::Float(x.round()))
        }
        _ if name.eq_ignore_ascii_case("sqrt") => {
            numeric_to_float(arg(0), span).map(|x| Value::Float(x.sqrt()))
        }
        _ if name.eq_ignore_ascii_case("sign") => match arg(0) {
            Value::Int(n) => Ok(Value::Int(n.signum())),
            Value::Float(x) => Ok(Value::Int(if x > 0.0 {
                1
            } else if x < 0.0 {
                -1
            } else {
                0
            })),
            other => Err(type_error(format!(
                "sign() needs a number, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("size") => match arg(0) {
            Value::List(items) => Ok(Value::Int(items.len() as i64)),
            Value::String(s) => Ok(Value::Int(s.chars().count() as i64)),
            Value::Map(map) => Ok(Value::Int(map.len() as i64)),
            other => Err(type_error(format!(
                "size() cannot measure {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("length") => match arg(0) {
            Value::Path(path) => Ok(Value::Int(path.rels.len() as i64)),
            Value::String(s) => Ok(Value::Int(s.chars().count() as i64)),
            Value::List(items) => Ok(Value::Int(items.len() as i64)),
            other => Err(type_error(format!(
                "length() cannot measure {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("head") => match arg(0) {
            Value::List(items) => Ok(items.into_iter().next().unwrap_or(Value::Null)),
            other => Err(type_error(format!(
                "head() needs a list, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("last") => match arg(0) {
            Value::List(items) => Ok(items.into_iter().next_back().unwrap_or(Value::Null)),
            other => Err(type_error(format!(
                "last() needs a list, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("tail") => match arg(0) {
            Value::List(items) => Ok(Value::List(items.into_iter().skip(1).collect())),
            other => Err(type_error(format!(
                "tail() needs a list, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("reverse") => match arg(0) {
            Value::List(mut items) => {
                items.reverse();
                Ok(Value::List(items))
            }
            Value::String(s) => Ok(Value::String(s.chars().rev().collect())),
            other => Err(type_error(format!(
                "reverse() cannot reverse {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("keys") => match arg(0) {
            Value::Map(map) => Ok(Value::List(map.into_keys().map(Value::String).collect())),
            Value::Node(node) => Ok(Value::List(
                node.properties.into_keys().map(Value::String).collect(),
            )),
            Value::Relationship(rel) => Ok(Value::List(
                rel.properties.into_keys().map(Value::String).collect(),
            )),
            other => Err(type_error(format!(
                "keys() cannot inspect {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("properties") => match arg(0) {
            Value::Map(map) => Ok(Value::Map(map)),
            Value::Node(node) => Ok(Value::Map(node.properties)),
            Value::Relationship(rel) => Ok(Value::Map(rel.properties)),
            other => Err(type_error(format!(
                "properties() cannot inspect {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("labels") => match arg(0) {
            Value::Node(node) => Ok(Value::List(
                node.labels.into_iter().map(Value::String).collect(),
            )),
            other => Err(type_error(format!(
                "labels() needs a node, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("type") => match arg(0) {
            Value::Relationship(rel) => Ok(Value::String(rel.rel_type)),
            other => Err(type_error(format!(
                "type() needs a relationship, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("startNode") => match arg(0) {
            Value::Relationship(rel) => Ok(graph
                .node(&rel.start)
                .map(Value::Node)
                .unwrap_or(Value::Null)),
            other => Err(type_error(format!(
                "startNode() needs a relationship, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("endNode") => match arg(0) {
            Value::Relationship(rel) => {
                Ok(graph.node(&rel.end).map(Value::Node).unwrap_or(Value::Null))
            }
            other => Err(type_error(format!(
                "endNode() needs a relationship, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("nodes") => match arg(0) {
            Value::Path(path) => Ok(Value::List(
                path.nodes.into_iter().map(Value::Node).collect(),
            )),
            other => Err(type_error(format!(
                "nodes() needs a path, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("relationships") => match arg(0) {
            Value::Path(path) => Ok(Value::List(
                path.rels.into_iter().map(Value::Relationship).collect(),
            )),
            other => Err(type_error(format!(
                "relationships() needs a path, got {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("range") => {
            let bounds: Vec<i64> = {
                let mut out = Vec::new();
                for value in &args {
                    match value {
                        Value::Int(n) => out.push(*n),
                        other => {
                            return Err(ExecError::InvalidArgument {
                                message: format!(
                                    "range() needs integers, got {}",
                                    other.type_name()
                                ),
                                span,
                            });
                        }
                    }
                }
                out
            };
            let step = bounds.get(2).copied().unwrap_or(1);
            if step == 0 {
                return Err(ExecError::InvalidArgument {
                    message: "range() step cannot be zero".into(),
                    span,
                });
            }
            // The binder validates range()'s arity; guard defensively anyway so
            // untrusted input can never index out of bounds.
            let (Some(&start), Some(&end)) = (bounds.first(), bounds.get(1)) else {
                return Err(ExecError::InvalidArgument {
                    message: "range() needs a start and an end".into(),
                    span,
                });
            };
            // Compute the element count in i128 so neither the count nor the
            // stepping can overflow i64, and cap it so a huge range cannot
            // exhaust memory (previously `at += step` could panic on overflow or
            // grow the list without bound).
            let span_len = i128::from(end) - i128::from(start);
            let count: i128 = if (step > 0 && span_len < 0) || (step < 0 && span_len > 0) {
                0
            } else {
                span_len / i128::from(step) + 1
            };
            if count > MAX_RANGE_ELEMENTS {
                return Err(ExecError::InvalidArgument {
                    message: format!(
                        "range() would produce {count} elements, exceeding the limit of \
                         {MAX_RANGE_ELEMENTS}"
                    ),
                    span,
                });
            }
            let count = count as usize;
            let mut items = Vec::with_capacity(count);
            let mut at = start;
            for i in 0..count {
                items.push(Value::Int(at));
                // Only advance between elements: every emitted value stays within
                // [start, end], so this never overflows i64.
                if i + 1 < count {
                    at += step;
                }
            }
            Ok(Value::List(items))
        }
        _ if name.eq_ignore_ascii_case("toUpper") => string_fn(arg(0), span, |s| s.to_uppercase()),
        _ if name.eq_ignore_ascii_case("toLower") => string_fn(arg(0), span, |s| s.to_lowercase()),
        _ if name.eq_ignore_ascii_case("trim") => string_fn(arg(0), span, |s| s.trim().to_string()),
        _ if name.eq_ignore_ascii_case("lTrim") => {
            string_fn(arg(0), span, |s| s.trim_start().to_string())
        }
        _ if name.eq_ignore_ascii_case("rTrim") => {
            string_fn(arg(0), span, |s| s.trim_end().to_string())
        }
        _ if name.eq_ignore_ascii_case("split") => match (arg(0), arg(1)) {
            (Value::String(s), Value::String(sep)) => Ok(Value::List(
                s.split(&sep)
                    .map(|part| Value::String(part.to_string()))
                    .collect(),
            )),
            (a, b) => Err(type_error(format!(
                "split() needs strings, got {} and {}",
                a.type_name(),
                b.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("replace") => match (arg(0), arg(1), arg(2)) {
            (Value::String(s), Value::String(from), Value::String(to)) => {
                Ok(Value::String(s.replace(&from, &to)))
            }
            _ => Err(type_error("replace() needs three strings".into())),
        },
        _ if name.eq_ignore_ascii_case("left") => match (arg(0), arg(1)) {
            (Value::String(s), Value::Int(n)) if n >= 0 => {
                Ok(Value::String(s.chars().take(n as usize).collect()))
            }
            (Value::String(_), Value::Int(_)) => Err(ExecError::InvalidArgument {
                message: "left() length cannot be negative".into(),
                span,
            }),
            _ => Err(type_error("left() needs a string and an integer".into())),
        },
        _ if name.eq_ignore_ascii_case("right") => match (arg(0), arg(1)) {
            (Value::String(s), Value::Int(n)) if n >= 0 => {
                let chars: Vec<char> = s.chars().collect();
                let start = chars.len().saturating_sub(n as usize);
                Ok(Value::String(chars[start..].iter().collect()))
            }
            (Value::String(_), Value::Int(_)) => Err(ExecError::InvalidArgument {
                message: "right() length cannot be negative".into(),
                span,
            }),
            _ => Err(type_error("right() needs a string and an integer".into())),
        },
        _ if name.eq_ignore_ascii_case("substring") => match (arg(0), arg(1)) {
            (Value::String(s), Value::Int(start)) if start >= 0 => {
                let chars: Vec<char> = s.chars().collect();
                let start = (start as usize).min(chars.len());
                let taken: String = match args.get(2) {
                    Some(Value::Int(len)) if *len >= 0 => {
                        chars[start..].iter().take(*len as usize).collect()
                    }
                    Some(Value::Int(_)) => {
                        return Err(ExecError::InvalidArgument {
                            message: "substring() length cannot be negative".into(),
                            span,
                        });
                    }
                    Some(other) => {
                        return Err(type_error(format!(
                            "substring() length must be an integer, got {}",
                            other.type_name()
                        )));
                    }
                    None => chars[start..].iter().collect(),
                };
                Ok(Value::String(taken))
            }
            (Value::String(_), Value::Int(_)) => Err(ExecError::InvalidArgument {
                message: "substring() start cannot be negative".into(),
                span,
            }),
            _ => Err(type_error(
                "substring() needs a string and an integer".into(),
            )),
        },
        _ if name.eq_ignore_ascii_case("toString") => match arg(0) {
            Value::String(s) => Ok(Value::String(s)),
            Value::Int(n) => Ok(Value::String(n.to_string())),
            Value::Float(x) => Ok(Value::String(format_float(x))),
            Value::Bool(b) => Ok(Value::String(b.to_string())),
            other => Err(type_error(format!(
                "toString() cannot convert {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("toInteger") => match arg(0) {
            Value::Int(n) => Ok(Value::Int(n)),
            Value::Float(x) => Ok(Value::Int(x.trunc() as i64)),
            Value::String(s) => Ok(s
                .trim()
                .parse::<i64>()
                .map(Value::Int)
                .or_else(|_| {
                    s.trim()
                        .parse::<f64>()
                        .map(|x| Value::Int(x.trunc() as i64))
                })
                .unwrap_or(Value::Null)),
            other => Err(type_error(format!(
                "toInteger() cannot convert {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("toFloat") => match arg(0) {
            Value::Int(n) => Ok(Value::Float(n as f64)),
            Value::Float(x) => Ok(Value::Float(x)),
            Value::String(s) => Ok(s
                .trim()
                .parse::<f64>()
                .map(Value::Float)
                .unwrap_or(Value::Null)),
            other => Err(type_error(format!(
                "toFloat() cannot convert {}",
                other.type_name()
            ))),
        },
        _ if name.eq_ignore_ascii_case("toBoolean") => match arg(0) {
            Value::Bool(b) => Ok(Value::Bool(b)),
            Value::String(s) => Ok(match s.trim().to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => Value::Null,
            }),
            other => Err(type_error(format!(
                "toBoolean() cannot convert {}",
                other.type_name()
            ))),
        },
        _ => Err(ExecError::Unsupported {
            feature: "function",
            span,
        }),
    }
}

fn numeric_to_float(value: Value, span: Span) -> Result<f64, ExecError> {
    match value {
        Value::Int(n) => Ok(n as f64),
        Value::Float(x) => Ok(x),
        other => Err(ExecError::Type {
            message: format!("expected a number, got {}", other.type_name()),
            span,
        }),
    }
}

fn string_fn(value: Value, span: Span, f: impl Fn(&str) -> String) -> Result<Value, ExecError> {
    match value {
        Value::String(s) => Ok(Value::String(f(&s))),
        other => Err(ExecError::Type {
            message: format!("expected a string, got {}", other.type_name()),
            span,
        }),
    }
}

/// Float rendering matching openCypher expectations (`1.0` not `1`).
pub fn format_float(x: f64) -> String {
    if x.is_nan() {
        "NaN".into()
    } else if x.is_infinite() {
        if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        }
    } else if x == x.trunc() && x.abs() < 1e15 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}
