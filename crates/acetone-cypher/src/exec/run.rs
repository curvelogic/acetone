//! Query execution: a clause pipeline over materialised row sets.
//!
//! Heuristic planning per spec §5.3 lives in the pattern matcher's anchor
//! choice (a bound variable or the most selective scan available); the
//! spec's scan/expand operators are the matcher's internals rather than
//! reified structs, and `IndexSeek` awaits physical index maps (Phase 5).
//! Both are recorded deviations on the bead. Row sets are materialised —
//! at workbench scale (spec §1) streaming buys nothing yet.
//!
//! Relationship uniqueness: per openCypher, a relationship is traversed
//! at most once per MATCH clause (across all its comma patterns and
//! within var-length expansions).

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

use crate::ast::{AtRef, Direction};
use crate::bind::bound::*;
use crate::exec::eval::{EvalCtx, ExecError, Row, eval, truth};
use crate::exec::source::{GraphSource, SingleVersion, VersionResolver};
use crate::exec::value::{EntityId, NodeValue, PathValue, RelValue, Value};
use crate::exec::write::{MutableGraph, WriteSummary};

/// A completed query's output.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// Side effects of any write clauses (all zero for a read query).
    pub stats: WriteSummary,
}

/// Execute against a single fixed graph. `AT <ref>` clauses are
/// unsupported on this path (no repository to resolve refs against) —
/// used by the TCK backend and executor tests.
pub fn execute(
    query: &BoundQuery,
    graph: &dyn GraphSource,
    parameters: &BTreeMap<String, Value>,
) -> Result<QueryResult, ExecError> {
    execute_versioned(query, &SingleVersion::new(graph), parameters)
}

/// Execute against a version resolver, so `MATCH ... AT <ref>` clauses
/// (spec §5.2) query the graph at that commit while the rest of the
/// query sees the base version.
///
/// Cross-version identity: a node bound in an `AT` clause carries its
/// values (labels, properties) as a snapshot of that version, but is
/// identified by its natural key (Load-Bearing Invariant #3), which is
/// stable across versions. So re-anchoring it in a later base-version
/// clause — `MATCH (h) AT old MATCH (h)-[:R]->(x)` — deterministically
/// walks the *base* topology from that node's identity: the old node's
/// values with current edges. This blend is the only coherent behaviour
/// under natural-key identity.
pub fn execute_versioned(
    query: &BoundQuery,
    resolver: &dyn VersionResolver,
    parameters: &BTreeMap<String, Value>,
) -> Result<QueryResult, ExecError> {
    let base = resolver.base();
    // Write clauses mutate an overlay over the base version; reads in later
    // clauses see it. `AT` clauses resolve their own read-only sources.
    let mut graph = MutableGraph::new(base);
    let mut rows = vec![Row::default()];
    let mut result = None;

    for clause in &query.clauses {
        match clause {
            BoundClause::Match {
                optional,
                patterns,
                at_ref,
                where_clause,
                span,
            } => {
                // A clause-group AT resolves this MATCH against another
                // version; the resolved source is held for this clause.
                let at_graph = match at_ref {
                    None => None,
                    Some(at) => {
                        let refspec = resolve_at_ref(at, parameters)?;
                        let at = resolver.at(&refspec).map_err(|message| {
                            ExecError::InvalidArgument {
                                message,
                                span: *span,
                            }
                        })?;
                        Some(at)
                    }
                };
                let clause_ctx = match &at_graph {
                    Some(at) => EvalCtx::new(at.as_ref(), parameters),
                    None => EvalCtx::new(&graph, parameters),
                };
                rows = match_clause(
                    rows,
                    *optional,
                    patterns,
                    where_clause.as_ref(),
                    &clause_ctx,
                )?;
            }
            BoundClause::Unwind { expr, alias, span } => {
                let ctx = EvalCtx::new(&graph, parameters);
                let mut out = Vec::new();
                for row in rows {
                    let list = eval(expr, &row, &ctx)?;
                    match list {
                        Value::Null => {} // UNWIND null produces no rows
                        Value::List(items) => {
                            for item in items {
                                let mut next = row.clone();
                                next.set(*alias, item);
                                out.push(next);
                            }
                        }
                        other => {
                            return Err(ExecError::Type {
                                message: format!("UNWIND needs a list, got {}", other.type_name()),
                                span: *span,
                            });
                        }
                    }
                }
                rows = out;
            }
            BoundClause::With(projection) => {
                let ctx = EvalCtx::new(&graph, parameters);
                rows = project(rows, projection, &ctx, true)?.1;
            }
            BoundClause::Return(projection) => {
                let ctx = EvalCtx::new(&graph, parameters);
                let (columns, projected) = project(rows, projection, &ctx, false)?;
                let output = projected
                    .into_iter()
                    .map(|row| {
                        projection
                            .items
                            .iter()
                            .map(|item| row.get(item.var))
                            .collect()
                    })
                    .collect();
                result = Some(QueryResult {
                    columns,
                    rows: output,
                    stats: WriteSummary::default(),
                });
                rows = Vec::new();
            }
            BoundClause::Create { patterns, .. } => {
                rows = create_clause(rows, patterns, &mut graph, parameters)?;
            }
            BoundClause::Call { span, .. } => {
                return Err(ExecError::Unsupported {
                    feature: "procedures (arrive with acetone-yzc.7)",
                    span: *span,
                });
            }
        }
    }
    let stats = graph.summary().clone();
    let mut result = result.unwrap_or(QueryResult {
        columns: Vec::new(),
        rows: Vec::new(),
        stats: WriteSummary::default(),
    });
    result.stats = stats;
    Ok(result)
}

// --- CREATE -----------------------------------------------------------------

/// Execute a CREATE clause: for each incoming row, create the unbound
/// elements of every pattern and bind the new variables (openCypher: each
/// row drives one instantiation; created elements are visible to later
/// clauses through the [`MutableGraph`] overlay).
fn create_clause(
    rows: Vec<Row>,
    patterns: &[BoundPathPattern],
    graph: &mut MutableGraph,
    parameters: &BTreeMap<String, Value>,
) -> Result<Vec<Row>, ExecError> {
    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        for pattern in patterns {
            create_path(pattern, &mut row, graph, parameters)?;
        }
        out.push(row);
    }
    Ok(out)
}

fn create_path(
    pattern: &BoundPathPattern,
    row: &mut Row,
    graph: &mut MutableGraph,
    parameters: &BTreeMap<String, Value>,
) -> Result<(), ExecError> {
    let start = resolve_or_create_node(&pattern.start, row, graph, parameters)?;
    let mut path_nodes = vec![start.clone()];
    let mut path_rels = Vec::new();
    let mut prev = start;

    for (rel_pattern, node_pattern) in &pattern.steps {
        let target = resolve_or_create_node(node_pattern, row, graph, parameters)?;
        // The binder guarantees exactly one type and a concrete direction.
        let rel_type = rel_pattern.types[0].clone();
        let props = {
            let ctx = EvalCtx::new(&*graph, parameters);
            eval_property_map(rel_pattern.properties.as_ref(), row, &ctx, rel_pattern.span)?
        };
        let (start_id, end_id) = match rel_pattern.direction {
            Direction::Out => (prev.id.clone(), target.id.clone()),
            Direction::In => (target.id.clone(), prev.id.clone()),
            Direction::Undirected => {
                return Err(ExecError::InvalidArgument {
                    message: "CREATE requires a directed relationship".into(),
                    span: rel_pattern.span,
                });
            }
        };
        let rel = graph.create_rel(start_id, rel_type, end_id, props);
        if let Some(var) = rel_pattern.var {
            row.set(var, Value::Relationship(rel.clone()));
        }
        path_rels.push(rel);
        path_nodes.push(target.clone());
        prev = target;
    }

    if let Some(var) = pattern.path_var {
        row.set(
            var,
            Value::Path(PathValue {
                nodes: path_nodes,
                rels: path_rels,
            }),
        );
    }
    Ok(())
}

/// A CREATE node position: an already-bound variable references an
/// existing node; a fresh (or anonymous) position creates one.
fn resolve_or_create_node(
    node: &BoundNodePattern,
    row: &mut Row,
    graph: &mut MutableGraph,
    parameters: &BTreeMap<String, Value>,
) -> Result<NodeValue, ExecError> {
    if let Some(var) = node.var
        && row.contains(var)
    {
        return match row.get(var) {
            Value::Node(existing) => Ok(existing),
            other => Err(ExecError::Type {
                message: format!(
                    "CREATE cannot reference a bound {} as a node",
                    other.type_name()
                ),
                span: node.span,
            }),
        };
    }
    let props = {
        let ctx = EvalCtx::new(&*graph, parameters);
        eval_property_map(node.properties.as_ref(), row, &ctx, node.span)?
    };
    let created = graph.create_node(node.labels.clone(), props);
    if let Some(var) = node.var {
        row.set(var, Value::Node(created.clone()));
    }
    Ok(created)
}

/// Evaluate a pattern property map (a map literal or a parameter) to a
/// concrete property map. An absent map is empty; a non-map value is an
/// error.
fn eval_property_map(
    properties: Option<&BoundExpr>,
    row: &Row,
    ctx: &EvalCtx,
    span: crate::span::Span,
) -> Result<BTreeMap<String, Value>, ExecError> {
    match properties {
        None => Ok(BTreeMap::new()),
        Some(expr) => match eval(expr, row, ctx)? {
            Value::Map(map) => Ok(map),
            other => Err(ExecError::Type {
                message: format!("a property map must be a map, got {}", other.type_name()),
                span,
            }),
        },
    }
}

/// Resolve a clause-group `AT` reference to a refspec string. A
/// parameter form (`AT $ref`) reads the parameter, which must be a
/// string.
fn resolve_at_ref(at: &AtRef, parameters: &BTreeMap<String, Value>) -> Result<String, ExecError> {
    match at {
        AtRef::Refspec { value, .. } => Ok(value.clone()),
        AtRef::Parameter { name, span } => match parameters.get(name) {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(other) => Err(ExecError::Type {
                message: format!("AT parameter must be a string, got {}", other.type_name()),
                span: *span,
            }),
            None => Err(ExecError::MissingParameter {
                name: name.clone(),
                span: *span,
            }),
        },
    }
}

// --- MATCH ------------------------------------------------------------------

struct MatchState {
    row: Row,
    used_rels: HashSet<EntityId>,
}

fn match_clause(
    rows: Vec<Row>,
    optional: bool,
    patterns: &[BoundPathPattern],
    where_clause: Option<&BoundExpr>,
    ctx: &EvalCtx,
) -> Result<Vec<Row>, ExecError> {
    // Variables this clause mentions; on an empty optional match, those
    // not already bound in the incoming row get nulls.
    let mut mentioned: Vec<VarId> = Vec::new();
    for pattern in patterns {
        if let Some(var) = pattern.path_var {
            mentioned.push(var);
        }
        mentioned.extend(pattern.start.var);
        for (rel, node) in &pattern.steps {
            mentioned.extend(rel.var);
            mentioned.extend(node.var);
        }
    }

    let mut out = Vec::new();
    for row in rows {
        let mut states = vec![MatchState {
            row: row.clone(),
            used_rels: HashSet::new(),
        }];
        for pattern in patterns {
            let mut next = Vec::new();
            for state in states {
                next.extend(match_path(pattern, state, ctx)?);
            }
            states = next;
        }
        // The WHERE participates in the match (essential for OPTIONAL).
        let mut matched = Vec::new();
        for state in states {
            if let Some(predicate) = where_clause
                && truth(&eval(predicate, &state.row, ctx)?, predicate.span())? != Some(true)
            {
                continue;
            }
            matched.push(state.row);
        }
        if matched.is_empty() && optional {
            let mut nulled = row;
            for var in &mentioned {
                if !nulled.contains(*var) {
                    nulled.set(*var, Value::Null);
                }
            }
            out.push(nulled);
        } else {
            out.extend(matched);
        }
    }
    Ok(out)
}

fn match_path(
    pattern: &BoundPathPattern,
    state: MatchState,
    ctx: &EvalCtx,
) -> Result<Vec<MatchState>, ExecError> {
    // Anchor at the leftmost node: a bound variable pins it; otherwise
    // scan by labels (the heuristic planner's LabelScan/AllNodesScan).
    let anchors: Vec<NodeValue> = match pattern.start.var {
        // Bound variable: pinned (bound null or non-node matches nothing).
        Some(var) if state.row.contains(var) => match state.row.get(var) {
            Value::Node(node) => vec![node],
            _ => return Ok(Vec::new()),
        },
        // Fresh or anonymous: scan (the heuristic LabelScan/AllNodesScan).
        _ => ctx.graph.nodes_by_labels(&pattern.start.labels),
    };

    let mut results = Vec::new();
    for anchor in anchors {
        if !node_satisfies(&anchor, &pattern.start, &state.row, ctx)? {
            continue;
        }
        let mut row = state.row.clone();
        if let Some(var) = pattern.start.var {
            row.set(var, Value::Node(anchor.clone()));
        }
        let path_state = PathBuild {
            nodes: vec![anchor.clone()],
            rels: Vec::new(),
        };
        walk_steps(
            pattern,
            0,
            anchor,
            MatchState {
                row,
                used_rels: state.used_rels.clone(),
            },
            path_state,
            ctx,
            &mut results,
        )?;
    }
    Ok(results)
}

#[derive(Clone)]
struct PathBuild {
    nodes: Vec<NodeValue>,
    rels: Vec<RelValue>,
}

#[allow(clippy::too_many_arguments)]
fn walk_steps(
    pattern: &BoundPathPattern,
    at: usize,
    from: NodeValue,
    state: MatchState,
    path: PathBuild,
    ctx: &EvalCtx,
    results: &mut Vec<MatchState>,
) -> Result<(), ExecError> {
    let Some((rel_pattern, node_pattern)) = pattern.steps.get(at) else {
        let mut done = state;
        if let Some(var) = pattern.path_var {
            done.row.set(
                var,
                Value::Path(PathValue {
                    nodes: path.nodes.clone(),
                    rels: path.rels.clone(),
                }),
            );
        }
        results.push(done);
        return Ok(());
    };

    match rel_pattern.var_length {
        None => {
            for (rel, neighbour) in
                ctx.graph
                    .expand(&from.id, rel_pattern.direction, &rel_pattern.types)
            {
                if state.used_rels.contains(&rel.id) {
                    continue;
                }
                if !rel_satisfies(&rel, rel_pattern, &state.row, ctx)? {
                    continue;
                }
                if !node_satisfies(&neighbour, node_pattern, &state.row, ctx)? {
                    continue;
                }
                let mut next = MatchState {
                    row: state.row.clone(),
                    used_rels: state.used_rels.clone(),
                };
                next.used_rels.insert(rel.id.clone());
                if let Some(var) = rel_pattern.var {
                    if next.row.contains(var) {
                        // Bound: an equality constraint (bound null or a
                        // non-relationship matches nothing).
                        match next.row.get(var) {
                            Value::Relationship(bound) if bound.id == rel.id => {}
                            _ => continue,
                        }
                    } else {
                        next.row.set(var, Value::Relationship(rel.clone()));
                    }
                }
                if let Some(var) = node_pattern.var {
                    if next.row.contains(var) {
                        match next.row.get(var) {
                            Value::Node(bound) if bound.id == neighbour.id => {}
                            _ => continue,
                        }
                    } else {
                        next.row.set(var, Value::Node(neighbour.clone()));
                    }
                }
                let mut next_path = path.clone();
                next_path.rels.push(rel.clone());
                next_path.nodes.push(neighbour.clone());
                walk_steps(pattern, at + 1, neighbour, next, next_path, ctx, results)?;
            }
        }
        Some(bounds) => {
            let min = bounds.min.unwrap_or(1) as usize;
            let max = bounds.max.map(|m| m as usize).unwrap_or(usize::MAX);
            expand_var_length(
                pattern,
                at,
                rel_pattern,
                node_pattern,
                from,
                state,
                path,
                Vec::new(),
                min,
                max,
                ctx,
                results,
            )?;
        }
    }
    Ok(())
}

/// Var-length expansion with relationship uniqueness pruning; at each
/// depth within bounds, try to close on the target node pattern.
#[allow(clippy::too_many_arguments)]
fn expand_var_length(
    pattern: &BoundPathPattern,
    at: usize,
    rel_pattern: &BoundRelPattern,
    node_pattern: &BoundNodePattern,
    from: NodeValue,
    state: MatchState,
    path: PathBuild,
    hops: Vec<RelValue>,
    min: usize,
    max: usize,
    ctx: &EvalCtx,
    results: &mut Vec<MatchState>,
) -> Result<(), ExecError> {
    if hops.len() >= min && node_satisfies(&from, node_pattern, &state.row, ctx)? {
        let mut next = MatchState {
            row: state.row.clone(),
            used_rels: state.used_rels.clone(),
        };
        if let Some(var) = rel_pattern.var {
            next.row.set(
                var,
                Value::List(hops.iter().cloned().map(Value::Relationship).collect()),
            );
        }
        let target_ok = match node_pattern.var {
            Some(var) if next.row.contains(var) => {
                matches!(next.row.get(var), Value::Node(bound) if bound.id == from.id)
            }
            Some(var) => {
                next.row.set(var, Value::Node(from.clone()));
                true
            }
            None => true,
        };
        if target_ok {
            walk_steps(
                pattern,
                at + 1,
                from.clone(),
                next,
                path.clone(),
                ctx,
                results,
            )?;
        }
    }
    if hops.len() >= max {
        return Ok(());
    }
    for (rel, neighbour) in ctx
        .graph
        .expand(&from.id, rel_pattern.direction, &rel_pattern.types)
    {
        if state.used_rels.contains(&rel.id) {
            continue;
        }
        if !rel_satisfies(&rel, rel_pattern, &state.row, ctx)? {
            continue;
        }
        let mut next_state = MatchState {
            row: state.row.clone(),
            used_rels: state.used_rels.clone(),
        };
        next_state.used_rels.insert(rel.id.clone());
        let mut next_path = path.clone();
        next_path.rels.push(rel.clone());
        next_path.nodes.push(neighbour.clone());
        let mut next_hops = hops.clone();
        next_hops.push(rel.clone());
        expand_var_length(
            pattern,
            at,
            rel_pattern,
            node_pattern,
            neighbour,
            next_state,
            next_path,
            next_hops,
            min,
            max,
            ctx,
            results,
        )?;
    }
    Ok(())
}

fn node_satisfies(
    node: &NodeValue,
    pattern: &BoundNodePattern,
    row: &Row,
    ctx: &EvalCtx,
) -> Result<bool, ExecError> {
    if !pattern.labels.iter().all(|l| node.labels.contains(l)) {
        return Ok(false);
    }
    // A bound node variable pins identity (bound null matches nothing).
    if let Some(var) = pattern.var
        && row.contains(var)
    {
        match row.get(var) {
            Value::Node(bound) if bound.id == node.id => {}
            _ => return Ok(false),
        }
    }
    properties_satisfy(&pattern.properties, &node.properties, row, ctx)
}

fn rel_satisfies(
    rel: &RelValue,
    pattern: &BoundRelPattern,
    row: &Row,
    ctx: &EvalCtx,
) -> Result<bool, ExecError> {
    properties_satisfy(&pattern.properties, &rel.properties, row, ctx)
}

fn properties_satisfy(
    pattern_properties: &Option<BoundExpr>,
    actual: &BTreeMap<String, Value>,
    row: &Row,
    ctx: &EvalCtx,
) -> Result<bool, ExecError> {
    let Some(expr) = pattern_properties else {
        return Ok(true);
    };
    let expected = eval(expr, row, ctx)?;
    let Value::Map(expected) = expected else {
        return Ok(false);
    };
    for (key, want) in &expected {
        let Some(have) = actual.get(key) else {
            return Ok(false);
        };
        if have.eq3(want) != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

// --- Projection (WITH / RETURN) ----------------------------------------------

/// Wrapper giving Value a total order (the global sort order) for use as
/// grouping keys.
#[derive(Debug, Clone)]
struct OrdValue(Value);

impl PartialEq for OrdValue {
    fn eq(&self, other: &Self) -> bool {
        self.0.equivalent(&other.0)
    }
}
impl Eq for OrdValue {}
impl PartialOrd for OrdValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.global_cmp(&other.0)
    }
}

fn project(
    rows: Vec<Row>,
    projection: &BoundProjection,
    ctx: &EvalCtx,
    is_with: bool,
) -> Result<(Vec<String>, Vec<Row>), ExecError> {
    let columns: Vec<String> = projection
        .items
        .iter()
        .map(|item| item.name.clone())
        .collect();

    // Rows carrying the projected values (and, merged, the pre-projection
    // slots so ORDER BY / WITH..WHERE can see the union scope).
    let mut merged: Vec<Row> = Vec::new();

    if projection.aggregating {
        // Group by the non-aggregating items' values.
        let mut groups: BTreeMap<Vec<OrdValue>, Vec<Row>> = BTreeMap::new();
        for row in rows {
            let mut key = Vec::new();
            for &index in &projection.grouping_items {
                let value = eval(&projection.items[index].expr, &row, ctx)?;
                key.push(OrdValue(value));
            }
            groups.entry(key).or_default().push(row);
        }
        // Aggregating over zero rows with no grouping keys still yields
        // one output row (count(*) = 0 and friends).
        if groups.is_empty() && projection.grouping_items.is_empty() {
            groups.insert(Vec::new(), Vec::new());
        }
        for (_, group) in groups {
            let representative = group.first().cloned().unwrap_or_default();
            let mut out = representative.clone();
            for item in &projection.items {
                let value = eval_with_group(&item.expr, &group, &representative, ctx)?;
                out.set(item.var, value);
            }
            merged.push(out);
        }
    } else {
        for row in rows {
            let mut out = row.clone();
            for item in &projection.items {
                let value = eval(&item.expr, &row, ctx)?;
                out.set(item.var, value);
            }
            merged.push(out);
        }
    }

    // WITH ... WHERE filters in the union scope.
    if is_with && let Some(predicate) = &projection.where_clause {
        let mut kept = Vec::new();
        for row in merged {
            if truth(&eval(predicate, &row, ctx)?, predicate.span())? == Some(true) {
                kept.push(row);
            }
        }
        merged = kept;
    }

    // DISTINCT over the projected values.
    if projection.distinct {
        let mut seen: Vec<Vec<OrdValue>> = Vec::new();
        let mut kept = Vec::new();
        for row in merged {
            let key: Vec<OrdValue> = projection
                .items
                .iter()
                .map(|item| OrdValue(row.get(item.var)))
                .collect();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            kept.push(row);
        }
        merged = kept;
    }

    // ORDER BY in the union scope, on the global sort order.
    if !projection.order_by.is_empty() {
        let mut keyed: Vec<(Vec<(OrdValue, bool)>, Row)> = Vec::new();
        for row in merged {
            let mut key = Vec::new();
            for (expr, descending) in &projection.order_by {
                key.push((OrdValue(eval(expr, &row, ctx)?), *descending));
            }
            keyed.push((key, row));
        }
        keyed.sort_by(|(a, _), (b, _)| {
            for ((x, descending), (y, _)) in a.iter().zip(b) {
                let ordering = x.cmp(y);
                let ordering = if *descending {
                    ordering.reverse()
                } else {
                    ordering
                };
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            std::cmp::Ordering::Equal
        });
        merged = keyed.into_iter().map(|(_, row)| row).collect();
    }

    // SKIP / LIMIT (constants or parameters).
    let constant = Row::default();
    if let Some(expr) = &projection.skip {
        let count = usize_bound(eval(expr, &constant, ctx)?, expr.span())?;
        merged = merged.into_iter().skip(count).collect();
    }
    if let Some(expr) = &projection.limit {
        let count = usize_bound(eval(expr, &constant, ctx)?, expr.span())?;
        merged.truncate(count);
    }

    Ok((columns, merged))
}

fn usize_bound(value: Value, span: crate::span::Span) -> Result<usize, ExecError> {
    match value {
        Value::Int(n) if n >= 0 => Ok(n as usize),
        other => Err(ExecError::InvalidArgument {
            message: format!("SKIP/LIMIT needs a non-negative integer, got {other:?}"),
            span,
        }),
    }
}

/// Evaluate a projection item over a group: aggregates are accumulated
/// across the group's rows and non-aggregate parts use a representative
/// row.
fn eval_with_group(
    expr: &BoundExpr,
    group: &[Row],
    representative: &Row,
    ctx: &EvalCtx,
) -> Result<Value, ExecError> {
    let mut aggregates = Vec::new();
    collect_aggregates(expr, &mut aggregates);
    let mut slots = Vec::with_capacity(aggregates.len());
    for aggregate in aggregates {
        slots.push(accumulate(aggregate, group, ctx)?);
    }
    let inner = EvalCtx {
        graph: ctx.graph,
        parameters: ctx.parameters,
        aggregates: Some((&slots, Cell::new(0))),
    };
    eval(expr, representative, &inner)
}

/// Enumerate Aggregate nodes in the same traversal order `eval` visits
/// them (depth-first, argument order). Aggregate arguments cannot contain
/// aggregates (binder-enforced), so no recursion into them.
fn collect_aggregates<'e>(expr: &'e BoundExpr, out: &mut Vec<&'e BoundExpr>) {
    match expr {
        BoundExpr::Aggregate { .. } => out.push(expr),
        BoundExpr::Literal { .. } | BoundExpr::Parameter { .. } | BoundExpr::Variable { .. } => {}
        BoundExpr::Property { base, .. } => collect_aggregates(base, out),
        BoundExpr::Unary { operand, .. } | BoundExpr::IsNull { operand, .. } => {
            collect_aggregates(operand, out);
        }
        BoundExpr::Binary { lhs, rhs, .. } => {
            collect_aggregates(lhs, out);
            collect_aggregates(rhs, out);
        }
        BoundExpr::Function { args, .. } => {
            for arg in args {
                collect_aggregates(arg, out);
            }
        }
        BoundExpr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            if let Some(operand) = operand {
                collect_aggregates(operand, out);
            }
            for (condition, value) in whens {
                collect_aggregates(condition, out);
                collect_aggregates(value, out);
            }
            if let Some(else_expr) = else_expr {
                collect_aggregates(else_expr, out);
            }
        }
        BoundExpr::ListLiteral { items, .. } => {
            for item in items {
                collect_aggregates(item, out);
            }
        }
        BoundExpr::ListComprehension {
            list,
            where_clause,
            map,
            ..
        } => {
            collect_aggregates(list, out);
            if let Some(expr) = where_clause {
                collect_aggregates(expr, out);
            }
            if let Some(expr) = map {
                collect_aggregates(expr, out);
            }
        }
        BoundExpr::Quantifier {
            list, predicate, ..
        } => {
            collect_aggregates(list, out);
            collect_aggregates(predicate, out);
        }
        BoundExpr::Reduce {
            init, list, expr, ..
        } => {
            collect_aggregates(init, out);
            collect_aggregates(list, out);
            collect_aggregates(expr, out);
        }
        BoundExpr::MapLiteral { entries, .. } => {
            for (_, value) in entries {
                collect_aggregates(value, out);
            }
        }
        BoundExpr::Index { base, index, .. } => {
            collect_aggregates(base, out);
            collect_aggregates(index, out);
        }
        BoundExpr::Slice { base, from, to, .. } => {
            collect_aggregates(base, out);
            if let Some(expr) = from {
                collect_aggregates(expr, out);
            }
            if let Some(expr) = to {
                collect_aggregates(expr, out);
            }
        }
        BoundExpr::PatternPredicate { .. } => {}
    }
}

fn accumulate(aggregate: &BoundExpr, group: &[Row], ctx: &EvalCtx) -> Result<Value, ExecError> {
    let BoundExpr::Aggregate {
        def,
        distinct,
        arg,
        span,
    } = aggregate
    else {
        unreachable!("collect_aggregates only yields Aggregate nodes");
    };
    // Gather the argument values, skipping nulls (openCypher aggregates
    // ignore null inputs; count(*) counts rows).
    let mut values = Vec::new();
    for row in group {
        match arg {
            None => values.push(Value::Int(1)), // count(*)
            Some(expr) => {
                let value = eval(expr, row, ctx)?;
                if !value.is_null() {
                    values.push(value);
                }
            }
        }
    }
    if *distinct {
        let mut unique: Vec<Value> = Vec::new();
        for value in values {
            if !unique.iter().any(|seen| seen.equivalent(&value)) {
                unique.push(value);
            }
        }
        values = unique;
    }

    match def.name {
        "count" => Ok(Value::Int(values.len() as i64)),
        "collect" => Ok(Value::List(values)),
        "sum" => {
            let mut int_sum = 0i64;
            let mut float_sum = 0.0f64;
            let mut is_float = false;
            for value in &values {
                match value {
                    Value::Int(n) => {
                        int_sum = int_sum
                            .checked_add(*n)
                            .ok_or(ExecError::Overflow { span: *span })?;
                    }
                    Value::Float(x) => {
                        is_float = true;
                        float_sum += x;
                    }
                    other => {
                        return Err(ExecError::Type {
                            message: format!("sum() needs numbers, got {}", other.type_name()),
                            span: *span,
                        });
                    }
                }
            }
            if is_float {
                Ok(Value::Float(float_sum + int_sum as f64))
            } else {
                Ok(Value::Int(int_sum))
            }
        }
        "avg" => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut total = 0.0f64;
            for value in &values {
                match value {
                    Value::Int(n) => total += *n as f64,
                    Value::Float(x) => total += x,
                    other => {
                        return Err(ExecError::Type {
                            message: format!("avg() needs numbers, got {}", other.type_name()),
                            span: *span,
                        });
                    }
                }
            }
            Ok(Value::Float(total / values.len() as f64))
        }
        "min" => Ok(values
            .into_iter()
            .min_by(|a, b| a.global_cmp(b))
            .unwrap_or(Value::Null)),
        "max" => Ok(values
            .into_iter()
            .max_by(|a, b| a.global_cmp(b))
            .unwrap_or(Value::Null)),
        other => Err(ExecError::Unsupported {
            feature: "aggregate",
            span: *span,
        })
        .inspect_err(|_e| {
            let _ = other;
        }),
    }
}
