//! Name resolution, scoping and validation of a parsed query against a
//! schema catalogue, lowering the AST to the bound IR.
//!
//! Recursion here mirrors expression nesting, which the parser bounds at
//! `MAX_AST_DEPTH` — the binder inherits that stack-safety guarantee and
//! never recurses deeper than the AST it is given.

use std::collections::HashMap;

use crate::ast;
use crate::bind::bound::*;
use crate::bind::catalogue::Catalogue;
use crate::bind::error::BindError;
use crate::span::Span;

/// How unknown labels, relationship types and properties are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindMode {
    /// Workbench default: the schema map is authoritative, so a label or
    /// relationship type it does not declare is a precise error, and a
    /// property undeclared by a label that declares a shape is too.
    Strict,
    /// Schema-free binding (TCK, ad-hoc graphs): unknown labels and types
    /// bind as valid-but-undeclared — openCypher read semantics, where
    /// matching an undeclared label simply yields nothing. Name/scope/
    /// aggregation rules still apply in full.
    Lenient,
}

/// Bind `query` against `catalogue`. `source` is the original query text
/// (for deriving output column names from expression spans).
pub fn bind(
    source: &str,
    query: &ast::Query,
    catalogue: &Catalogue,
    mode: BindMode,
) -> Result<BoundQuery, BindError> {
    let mut binder = Binder {
        source,
        catalogue,
        mode,
        variables: Vec::new(),
        scope: HashMap::new(),
    };
    let mut clauses = Vec::new();
    for clause in &query.clauses {
        clauses.push(binder.clause(clause)?);
    }
    Ok(BoundQuery {
        clauses,
        variables: binder.variables,
    })
}

struct Binder<'a> {
    source: &'a str,
    catalogue: &'a Catalogue,
    mode: BindMode,
    variables: Vec<VarBinding>,
    /// Names currently visible.
    scope: HashMap<String, VarId>,
}

/// Expression context: where aggregates may appear.
#[derive(Clone, Copy)]
struct ExprCtx {
    aggregates_allowed: bool,
    in_aggregate: bool,
}

const NO_AGG: ExprCtx = ExprCtx {
    aggregates_allowed: false,
    in_aggregate: false,
};
const AGG_OK: ExprCtx = ExprCtx {
    aggregates_allowed: true,
    in_aggregate: false,
};

impl<'a> Binder<'a> {
    fn declare(&mut self, name: &str, kind: EntityKind, labels: Vec<String>) -> VarId {
        let id = VarId(self.variables.len() as u32);
        self.variables.push(VarBinding {
            id,
            name: name.to_string(),
            kind,
            labels,
        });
        self.scope.insert(name.to_string(), id);
        id
    }

    fn kind_of(&self, id: VarId) -> EntityKind {
        self.variables[id.0 as usize].kind
    }

    /// Restore a name to its pre-shadow binding (or remove it) after a
    /// scoped sub-expression (comprehension/quantifier/reduce variable).
    fn restore(&mut self, name: &str, shadowed: Option<VarId>) {
        match shadowed {
            Some(outer) => {
                self.scope.insert(name.to_string(), outer);
            }
            None => {
                self.scope.remove(name);
            }
        }
    }

    // --- clauses ---------------------------------------------------------

    fn clause(&mut self, clause: &ast::Clause) -> Result<BoundClause, BindError> {
        match clause {
            ast::Clause::Match(m) => self.match_clause(m),
            ast::Clause::Unwind(u) => {
                let expr = self.expr(&u.expr, NO_AGG)?;
                // openCypher: an UNWIND alias cannot shadow a bound name.
                if self.scope.contains_key(&u.alias) {
                    return Err(BindError::VariableAlreadyBound {
                        name: u.alias.clone(),
                        span: u.span,
                    });
                }
                let alias = self.declare(&u.alias, EntityKind::Value, vec![]);
                Ok(BoundClause::Unwind {
                    expr,
                    alias,
                    span: u.span,
                })
            }
            ast::Clause::With(p) => {
                let projection = self.projection(p, true)?;
                Ok(BoundClause::With(projection))
            }
            ast::Clause::Return(p) => {
                let projection = self.projection(p, false)?;
                Ok(BoundClause::Return(projection))
            }
            ast::Clause::Call(c) => self.call_clause(c),
            ast::Clause::Create(c) => self.create_clause(c),
            ast::Clause::Set(s) => self.set_clause(s),
            ast::Clause::Remove(r) => self.remove_clause(r),
            ast::Clause::Delete(d) => {
                let mut targets = Vec::new();
                for target in &d.targets {
                    targets.push(self.expr(target, NO_AGG)?);
                }
                Ok(BoundClause::Delete {
                    detach: d.detach,
                    targets,
                    span: d.span,
                })
            }
            ast::Clause::Merge(m) => self.merge_clause(m),
        }
    }

    fn merge_clause(&mut self, m: &ast::MergeClause) -> Result<BoundClause, BindError> {
        // The MERGE pattern may be created, so it obeys the CREATE rules
        // (directed single-typed relationships, fresh relationship
        // variables); its variables are then in scope for ON CREATE/MATCH.
        let pattern = self.create_pattern(&m.pattern)?;
        let mut on_create = Vec::new();
        for item in &m.on_create {
            on_create.push(self.set_item(item)?);
        }
        let mut on_match = Vec::new();
        for item in &m.on_match {
            on_match.push(self.set_item(item)?);
        }
        Ok(BoundClause::Merge {
            pattern,
            on_create,
            on_match,
            span: m.span,
        })
    }

    fn set_clause(&mut self, s: &ast::SetClause) -> Result<BoundClause, BindError> {
        let mut items = Vec::new();
        for item in &s.items {
            items.push(self.set_item(item)?);
        }
        Ok(BoundClause::Set {
            items,
            span: s.span,
        })
    }

    fn set_item(&mut self, item: &ast::SetItem) -> Result<BoundSetItem, BindError> {
        match item {
            ast::SetItem::Property {
                var,
                key,
                value,
                span,
            } => {
                let target = self.entity_target(var, *span, true)?;
                self.reject_key_property(target, key, *span)?;
                let value = self.expr(value, NO_AGG)?;
                Ok(BoundSetItem::Property {
                    target,
                    key: key.clone(),
                    value,
                    span: *span,
                })
            }
            ast::SetItem::Replace { var, value, span } => {
                let target = self.entity_target(var, *span, true)?;
                // Replacing the whole map would wipe key properties.
                if let Some((label, property)) = self.keyed_label(target) {
                    return Err(BindError::SetKeyProperty {
                        label,
                        property,
                        span: *span,
                    });
                }
                let value = self.expr(value, NO_AGG)?;
                Ok(BoundSetItem::Replace {
                    target,
                    value,
                    span: *span,
                })
            }
            ast::SetItem::Merge { var, value, span } => {
                let target = self.entity_target(var, *span, true)?;
                // A `+=` map literal that names a key property is rejected;
                // a parameter map is checked at run time (mex.3).
                if let ast::Expr::MapLiteral { entries, .. } = value {
                    for (property, _) in entries {
                        self.reject_key_property(target, property, *span)?;
                    }
                }
                let value = self.expr(value, NO_AGG)?;
                Ok(BoundSetItem::Merge {
                    target,
                    value,
                    span: *span,
                })
            }
            ast::SetItem::AddLabels { var, labels, span } => {
                let target = self.entity_target(var, *span, false)?;
                if self.mode == BindMode::Strict {
                    for label in labels {
                        if self.catalogue.label(label).is_none() {
                            return Err(BindError::UnknownLabel {
                                name: label.clone(),
                                span: *span,
                            });
                        }
                    }
                }
                Ok(BoundSetItem::AddLabels {
                    target,
                    labels: labels.clone(),
                    span: *span,
                })
            }
        }
    }

    fn remove_clause(&mut self, r: &ast::RemoveClause) -> Result<BoundClause, BindError> {
        let mut items = Vec::new();
        for item in &r.items {
            items.push(match item {
                ast::RemoveItem::Property { var, key, span } => {
                    let target = self.entity_target(var, *span, true)?;
                    self.reject_key_property(target, key, *span)?;
                    BoundRemoveItem::Property {
                        target,
                        key: key.clone(),
                        span: *span,
                    }
                }
                ast::RemoveItem::Labels { var, labels, span } => {
                    let target = self.entity_target(var, *span, false)?;
                    BoundRemoveItem::Labels {
                        target,
                        labels: labels.clone(),
                        span: *span,
                    }
                }
            });
        }
        Ok(BoundClause::Remove {
            items,
            span: r.span,
        })
    }

    /// Resolve a SET/REMOVE target variable. It must be in scope and denote
    /// an entity: a node, a relationship (only when `allow_rel`), or a
    /// dynamically-typed value (the executor re-checks). Label operations
    /// pass `allow_rel = false` — a relationship carries no labels.
    fn entity_target(&self, name: &str, span: Span, allow_rel: bool) -> Result<VarId, BindError> {
        let Some(&id) = self.scope.get(name) else {
            return Err(BindError::UndefinedVariable {
                name: name.to_string(),
                span,
            });
        };
        match self.kind_of(id) {
            EntityKind::Node | EntityKind::Value => Ok(id),
            EntityKind::Relationship if allow_rel => Ok(id),
            kind => Err(BindError::VariableTypeConflict {
                name: name.to_string(),
                expected: if allow_rel {
                    "node or relationship"
                } else {
                    EntityKind::Node.describe()
                },
                actual: kind.describe(),
                span,
            }),
        }
    }

    /// In Strict mode, reject touching a key property of a statically-known
    /// label (Invariant #3; the runtime case where the label is unknown is
    /// enforced later, mex.3).
    fn reject_key_property(
        &self,
        target: VarId,
        property: &str,
        span: Span,
    ) -> Result<(), BindError> {
        if self.mode != BindMode::Strict {
            return Ok(());
        }
        for label in &self.variables[target.0 as usize].labels {
            if self.catalogue.is_key_property(label, property) {
                return Err(BindError::SetKeyProperty {
                    label: label.clone(),
                    property: property.to_string(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// The first (label, key-property) of a statically-known keyed label on
    /// `target`, in Strict mode — used to reject whole-map replacement.
    fn keyed_label(&self, target: VarId) -> Option<(String, String)> {
        if self.mode != BindMode::Strict {
            return None;
        }
        for label in &self.variables[target.0 as usize].labels {
            if let Some(def) = self.catalogue.label(label)
                && let Some(key) = def.key().first()
            {
                return Some((label.clone(), key.clone()));
            }
        }
        None
    }

    fn create_clause(&mut self, c: &ast::CreateClause) -> Result<BoundClause, BindError> {
        let mut patterns = Vec::new();
        for pattern in &c.patterns {
            patterns.push(self.create_pattern(pattern)?);
        }
        Ok(BoundClause::Create {
            patterns,
            span: c.span,
        })
    }

    /// Bind a CREATE path pattern. Node variables follow ordinary
    /// introduce-rules (bound → referenced, fresh → created); relationship
    /// variables must be fresh and directed, with exactly one type and no
    /// var-length (openCypher CREATE restrictions).
    fn create_pattern(
        &mut self,
        pattern: &ast::PathPattern,
    ) -> Result<BoundPathPattern, BindError> {
        let path_var = match &pattern.variable {
            Some(name) => {
                if self.scope.contains_key(name) {
                    return Err(BindError::VariableAlreadyBound {
                        name: name.clone(),
                        span: pattern.span,
                    });
                }
                Some(self.declare(name, EntityKind::Path, vec![]))
            }
            None => None,
        };
        let start = self.create_node_pattern(&pattern.start)?;
        let mut steps = Vec::new();
        for (rel, node) in &pattern.steps {
            let rel = self.create_rel_pattern(rel)?;
            let node = self.create_node_pattern(node)?;
            steps.push((rel, node));
        }
        Ok(BoundPathPattern {
            path_var,
            start,
            steps,
            span: pattern.span,
        })
    }

    /// Bind a CREATE node position. A fresh (or anonymous) position is
    /// created; an already-bound variable is *referenced*, but openCypher
    /// forbids attaching labels or properties to that reference (that is a
    /// SET, not a CREATE) — silently dropping them would be a conformance
    /// gap, so it is a bind-time error.
    fn create_node_pattern(
        &mut self,
        node: &ast::NodePattern,
    ) -> Result<BoundNodePattern, BindError> {
        if let Some(name) = &node.variable
            && self.scope.contains_key(name)
            && (!node.labels.is_empty() || node.properties.is_some())
        {
            return Err(BindError::CreateBoundNodeWithProperties {
                name: name.clone(),
                span: node.span,
            });
        }
        self.node_pattern(node, true)
    }

    fn create_rel_pattern(&mut self, rel: &ast::RelPattern) -> Result<BoundRelPattern, BindError> {
        if rel.var_length.is_some() {
            return Err(BindError::CreateVarLengthRelationship { span: rel.span });
        }
        if rel.direction == ast::Direction::Undirected {
            return Err(BindError::CreateRequiresDirectedRelationship { span: rel.span });
        }
        if rel.types.len() != 1 {
            return Err(BindError::CreateRequiresSingleRelType { span: rel.span });
        }
        if self.mode == BindMode::Strict {
            for rel_type in &rel.types {
                if self.catalogue.rel_type(rel_type).is_none() {
                    return Err(BindError::UnknownRelType {
                        name: rel_type.clone(),
                        span: rel.span,
                    });
                }
            }
        }
        // A created relationship needs a fresh variable — reusing a bound
        // one would be an equality constraint, which CREATE cannot express.
        let var = match &rel.variable {
            Some(name) => {
                if self.scope.contains_key(name) {
                    return Err(BindError::VariableAlreadyBound {
                        name: name.clone(),
                        span: rel.span,
                    });
                }
                Some(self.declare(name, EntityKind::Relationship, vec![]))
            }
            None => None,
        };
        let properties = match &rel.properties {
            Some(expr) => Some(self.expr(expr, NO_AGG)?),
            None => None,
        };
        Ok(BoundRelPattern {
            var,
            types: rel.types.clone(),
            direction: rel.direction,
            var_length: rel.var_length,
            properties,
            span: rel.span,
        })
    }

    fn match_clause(&mut self, m: &ast::MatchClause) -> Result<BoundClause, BindError> {
        let mut patterns = Vec::new();
        for pattern in &m.patterns {
            patterns.push(self.path_pattern(pattern, true)?);
        }
        let where_clause = match &m.where_clause {
            Some(expr) => Some(self.expr(expr, NO_AGG)?),
            None => None,
        };
        Ok(BoundClause::Match {
            optional: m.optional,
            patterns,
            at_ref: m.at_ref.clone(),
            where_clause,
            span: m.span,
        })
    }

    fn call_clause(&mut self, c: &ast::CallClause) -> Result<BoundClause, BindError> {
        let name = c.procedure.join(".");
        let Some(def) = lookup_procedure(&name) else {
            return Err(BindError::ProcedureNotFound { name, span: c.span });
        };
        if c.args.len() < def.min_args || c.args.len() > def.max_args {
            return Err(BindError::InvalidNumberOfArguments {
                name,
                expected: if def.min_args == def.max_args {
                    format!("{}", def.min_args)
                } else {
                    format!("{}..{}", def.min_args, def.max_args)
                },
                got: c.args.len(),
                span: c.span,
            });
        }
        let mut args = Vec::new();
        for arg in &c.args {
            args.push(self.expr(arg, NO_AGG)?);
        }
        let mut yields = Vec::new();
        for column in &c.yield_items {
            if !def.yields.contains(&column.as_str()) {
                return Err(BindError::UnknownYieldColumn {
                    procedure: name.clone(),
                    column: column.clone(),
                    span: c.span,
                });
            }
            // A yield column cannot shadow a bound name (TCK Call1
            // [15]) nor repeat (Call5 [5][6]) — both VariableAlreadyBound
            // in TCK vocabulary.
            if self.scope.contains_key(column.as_str())
                || yields.iter().any(|(existing, _)| existing == column)
            {
                return Err(BindError::VariableAlreadyBound {
                    name: column.clone(),
                    span: c.span,
                });
            }
            let id = self.declare(column, EntityKind::Value, vec![]);
            yields.push((column.clone(), id));
        }
        let where_clause = match &c.where_clause {
            Some(expr) => Some(self.expr(expr, NO_AGG)?),
            None => None,
        };
        Ok(BoundClause::Call {
            procedure: def,
            args,
            yields,
            where_clause,
            span: c.span,
        })
    }

    fn projection(
        &mut self,
        p: &ast::Projection,
        is_with: bool,
    ) -> Result<BoundProjection, BindError> {
        // Bind item expressions in the current (pre-projection) scope.
        let mut bound_items: Vec<(BoundExpr, String, Span)> = Vec::new();
        for item in &p.items {
            match item {
                ast::ProjectionItem::Star { span } => {
                    // `*` projects every visible variable, by name order.
                    let mut names: Vec<&String> = self.scope.keys().collect();
                    if names.is_empty() {
                        return Err(BindError::NoVariablesInScope { span: *span });
                    }
                    names.sort();
                    for name in names.into_iter().cloned().collect::<Vec<_>>() {
                        let id = self.scope[&name];
                        bound_items.push((BoundExpr::Variable { id, span: *span }, name, *span));
                    }
                }
                ast::ProjectionItem::Expr { expr, alias, span } => {
                    let name = match alias {
                        Some(alias) => alias.clone(),
                        None => {
                            // WITH requires an alias unless the item is a
                            // plain variable; RETURN derives the column
                            // name from the expression text.
                            if is_with && !matches!(expr, ast::Expr::Variable { .. }) {
                                return Err(BindError::NoExpressionAlias { span: *span });
                            }
                            let expr_span = expr.span();
                            self.source[expr_span.start..expr_span.end].to_string()
                        }
                    };
                    let bound = self.expr(expr, AGG_OK)?;
                    bound_items.push((bound, name, *span));
                }
            }
        }
        // Column names must be unique.
        for (index, (_, name, span)) in bound_items.iter().enumerate() {
            if bound_items[..index]
                .iter()
                .any(|(_, other, _)| other == name)
            {
                return Err(BindError::ColumnNameConflict {
                    name: name.clone(),
                    span: *span,
                });
            }
        }

        let aggregating = bound_items
            .iter()
            .any(|(expr, _, _)| contains_aggregate(expr));
        let grouping_items = bound_items
            .iter()
            .enumerate()
            .filter(|(_, (expr, _, _))| !contains_aggregate(expr))
            .map(|(index, _)| index)
            .collect();

        // SKIP/LIMIT bind before re-scoping and cannot aggregate.
        let skip = match &p.skip {
            Some(expr) => Some(self.expr(expr, NO_AGG)?),
            None => None,
        };
        let limit = match &p.limit {
            Some(expr) => Some(self.expr(expr, NO_AGG)?),
            None => None,
        };

        // ORDER BY sees both the pre-projection scope and the new output
        // names; bind it against the union, then re-scope.
        let mut new_scope: HashMap<String, VarId> = HashMap::new();
        let mut items = Vec::new();
        for (expr, name, span) in bound_items {
            // A plain variable projection (`WITH n`, `WITH n AS m`) keeps
            // the entity's kind and labels — re-matching a projected node
            // or relationship is ordinary openCypher.
            let (kind, labels) = match &expr {
                BoundExpr::Variable { id, .. } => {
                    let source = &self.variables[id.0 as usize];
                    (source.kind, source.labels.clone())
                }
                _ => (EntityKind::Value, vec![]),
            };
            let id = VarId(self.variables.len() as u32);
            self.variables.push(VarBinding {
                id,
                name: name.clone(),
                kind,
                labels,
            });
            new_scope.insert(name.clone(), id);
            items.push(BoundProjectionItem {
                expr,
                name,
                var: id,
                span,
            });
        }
        // ORDER BY and WITH ... WHERE see both the pre-projection scope
        // and the new output names (openCypher: "WHERE sees a variable
        // bound before but not after WITH" is valid); only afterwards
        // does the scope narrow to the projected names.
        //
        // CONCEDED LIMITS (bead acetone-1qj): this union scope is only
        // correct for non-aggregating, non-DISTINCT projections. The
        // binder currently over-accepts, deferring to later phases, four
        // forms the TCK pins as compile-time errors: aggregates in ORDER
        // BY after a non-aggregating RETURN (ReturnOrderBy2 [14],
        // InvalidAggregation); ORDER BY on non-projected names after
        // DISTINCT (ReturnOrderBy2 [13], UndefinedVariable);
        // post-aggregation ORDER BY seeing the pre-scope (ReturnOrderBy6
        // [4], UndefinedVariable); and projection items mixing aggregates
        // with unaggregated pre-scope variables (With6 [8][9],
        // AmbiguousAggregationExpression) — for which grouping_items also
        // carries no planner signal yet. All classification-honest
        // (never credited as Passed).
        let union_scope: HashMap<String, VarId> = self
            .scope
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .chain(new_scope.iter().map(|(k, v)| (k.clone(), *v)))
            .collect();
        self.scope = union_scope;
        let order_by_result: Result<Vec<(BoundExpr, bool)>, BindError> = p
            .order_by
            .iter()
            .map(|sort| Ok((self.expr(&sort.expr, AGG_OK)?, sort.descending)))
            .collect();
        let order_by = order_by_result?;
        let where_clause = match &p.where_clause {
            Some(expr) => Some(self.expr(expr, NO_AGG)?),
            None => None,
        };

        // Re-scope: only the projected names survive.
        self.scope = new_scope;

        Ok(BoundProjection {
            distinct: p.distinct,
            items,
            order_by,
            skip,
            limit,
            where_clause,
            grouping_items,
            aggregating,
            span: p.span,
        })
    }

    // --- patterns ----------------------------------------------------------

    fn path_pattern(
        &mut self,
        pattern: &ast::PathPattern,
        introduce: bool,
    ) -> Result<BoundPathPattern, BindError> {
        let path_var = match &pattern.variable {
            Some(name) => {
                if self.scope.contains_key(name) {
                    return Err(BindError::VariableAlreadyBound {
                        name: name.clone(),
                        span: pattern.span,
                    });
                }
                if !introduce {
                    return Err(BindError::NewVariableInPatternPredicate {
                        name: name.clone(),
                        span: pattern.span,
                    });
                }
                Some(self.declare(name, EntityKind::Path, vec![]))
            }
            None => None,
        };
        let start = self.node_pattern(&pattern.start, introduce)?;
        let mut steps = Vec::new();
        for (rel, node) in &pattern.steps {
            let rel = self.rel_pattern(rel, introduce)?;
            let node = self.node_pattern(node, introduce)?;
            steps.push((rel, node));
        }
        Ok(BoundPathPattern {
            path_var,
            start,
            steps,
            span: pattern.span,
        })
    }

    fn node_pattern(
        &mut self,
        node: &ast::NodePattern,
        introduce: bool,
    ) -> Result<BoundNodePattern, BindError> {
        if self.mode == BindMode::Strict {
            for label in &node.labels {
                if self.catalogue.label(label).is_none() {
                    return Err(BindError::UnknownLabel {
                        name: label.clone(),
                        span: node.span,
                    });
                }
            }
        }
        let var = match &node.variable {
            Some(name) => match self.scope.get(name) {
                Some(&id) => {
                    let kind = self.kind_of(id);
                    match kind {
                        // Values may hold nodes at run time (UNWIND
                        // elements, coalesce results, nulls) — dynamic
                        // typing; the executor re-checks.
                        EntityKind::Node | EntityKind::Value => {}
                        EntityKind::Relationship | EntityKind::RelationshipList => {
                            return Err(BindError::VariableTypeConflict {
                                name: name.clone(),
                                expected: EntityKind::Node.describe(),
                                actual: kind.describe(),
                                span: node.span,
                            });
                        }
                        // Paths rebind nowhere, per openCypher.
                        EntityKind::Path => {
                            return Err(BindError::VariableAlreadyBound {
                                name: name.clone(),
                                span: node.span,
                            });
                        }
                    }
                    Some(id)
                }
                None => {
                    if !introduce {
                        return Err(BindError::NewVariableInPatternPredicate {
                            name: name.clone(),
                            span: node.span,
                        });
                    }
                    Some(self.declare(name, EntityKind::Node, node.labels.clone()))
                }
            },
            None => None,
        };
        let properties = match &node.properties {
            Some(expr) => {
                self.check_declared_properties(&node.labels, expr)?;
                Some(self.expr(expr, NO_AGG)?)
            }
            None => None,
        };
        let index_hint = self.index_hint(node);
        Ok(BoundNodePattern {
            var,
            labels: node.labels.clone(),
            properties,
            index_hint,
            span: node.span,
        })
    }

    fn rel_pattern(
        &mut self,
        rel: &ast::RelPattern,
        introduce: bool,
    ) -> Result<BoundRelPattern, BindError> {
        if self.mode == BindMode::Strict {
            for rel_type in &rel.types {
                if self.catalogue.rel_type(rel_type).is_none() {
                    return Err(BindError::UnknownRelType {
                        name: rel_type.clone(),
                        span: rel.span,
                    });
                }
            }
        }
        let var = match &rel.variable {
            Some(name) => match self.scope.get(name) {
                // A bound relationship (or list, or projected value) may
                // reappear in a later pattern as an equality constraint —
                // ordinary openCypher. Nodes and paths in relationship
                // position are conflicts.
                Some(&id) => {
                    let kind = self.kind_of(id);
                    match kind {
                        EntityKind::Relationship
                        | EntityKind::RelationshipList
                        | EntityKind::Value => Some(id),
                        EntityKind::Node => {
                            return Err(BindError::VariableTypeConflict {
                                name: name.clone(),
                                expected: EntityKind::Relationship.describe(),
                                actual: kind.describe(),
                                span: rel.span,
                            });
                        }
                        // Paths rebind nowhere, per openCypher.
                        EntityKind::Path => {
                            return Err(BindError::VariableAlreadyBound {
                                name: name.clone(),
                                span: rel.span,
                            });
                        }
                    }
                }
                None => {
                    if !introduce {
                        return Err(BindError::NewVariableInPatternPredicate {
                            name: name.clone(),
                            span: rel.span,
                        });
                    }
                    let kind = if rel.var_length.is_some() {
                        EntityKind::RelationshipList
                    } else {
                        EntityKind::Relationship
                    };
                    Some(self.declare(name, kind, vec![]))
                }
            },
            None => None,
        };
        let properties = match &rel.properties {
            Some(expr) => Some(self.expr(expr, NO_AGG)?),
            None => None,
        };
        Ok(BoundRelPattern {
            var,
            types: rel.types.clone(),
            direction: rel.direction,
            var_length: rel.var_length,
            properties,
            span: rel.span,
        })
    }

    /// Strict mode: a property map on a node whose label declares a shape
    /// may only use declared property names.
    ///
    /// Deliberate narrowing of the bead's recorded design (noted at bead
    /// close, tracked in acetone-1qj): property ACCESS expressions
    /// (`n.zzz` in WHERE/RETURN) are not checked — openCypher property
    /// access on a missing property yields null, and flagging it belongs
    /// with the workbench lint surface, not hard binding errors.
    fn check_declared_properties(
        &self,
        labels: &[String],
        properties: &ast::Expr,
    ) -> Result<(), BindError> {
        if self.mode != BindMode::Strict {
            return Ok(());
        }
        let ast::Expr::MapLiteral { entries, span } = properties else {
            return Ok(()); // parameter property maps are checked at run time
        };
        for label in labels {
            let Some(def) = self.catalogue.label(label) else {
                continue;
            };
            if def.types().is_empty() {
                continue; // shapeless label: any property is allowed
            }
            for (property, _) in entries {
                let declared =
                    def.types().contains_key(property) || def.key().iter().any(|k| k == property);
                if !declared {
                    return Err(BindError::UnknownProperty {
                        label: label.clone(),
                        property: property.clone(),
                        span: *span,
                    });
                }
            }
        }
        Ok(())
    }

    /// Planner hint: does the pattern's property map pin the leading key
    /// property (KeySeek on the primary map) or an indexed property
    /// (IndexSeek)? Only constant-ish values (literals, parameters) count.
    fn index_hint(&self, node: &ast::NodePattern) -> Option<IndexHint> {
        let [label] = node.labels.as_slice() else {
            return None;
        };
        let ast::Expr::MapLiteral { entries, .. } = node.properties.as_ref()? else {
            return None;
        };
        let pinned: Vec<&str> = entries
            .iter()
            .filter(|(_, value)| {
                matches!(
                    value,
                    ast::Expr::Literal { .. } | ast::Expr::Parameter { .. }
                )
            })
            .map(|(name, _)| name.as_str())
            .collect();
        if pinned.is_empty() {
            return None;
        }
        if pinned
            .iter()
            .any(|p| self.catalogue.is_key_prefix(label, p))
        {
            return Some(IndexHint::KeySeek {
                label: label.clone(),
            });
        }
        for property in pinned {
            if let Some((name, _)) = self.catalogue.index_on(label, property) {
                return Some(IndexHint::IndexSeek {
                    name: name.to_string(),
                    label: label.clone(),
                    property: property.to_string(),
                });
            }
        }
        None
    }

    // --- expressions ---------------------------------------------------------

    fn expr(&mut self, expr: &ast::Expr, ctx: ExprCtx) -> Result<BoundExpr, BindError> {
        match expr {
            ast::Expr::Literal { value, span } => Ok(BoundExpr::Literal {
                value: value.clone(),
                span: *span,
            }),
            ast::Expr::Parameter { name, span } => Ok(BoundExpr::Parameter {
                name: name.clone(),
                span: *span,
            }),
            ast::Expr::Variable { name, span } => match self.scope.get(name) {
                Some(&id) => Ok(BoundExpr::Variable { id, span: *span }),
                None => Err(BindError::UndefinedVariable {
                    name: name.clone(),
                    span: *span,
                }),
            },
            ast::Expr::Property { base, key, span } => Ok(BoundExpr::Property {
                base: Box::new(self.expr(base, ctx)?),
                key: key.clone(),
                span: *span,
            }),
            ast::Expr::Unary { op, operand, span } => Ok(BoundExpr::Unary {
                op: *op,
                operand: Box::new(self.expr(operand, ctx)?),
                span: *span,
            }),
            ast::Expr::Binary { op, lhs, rhs, span } => Ok(BoundExpr::Binary {
                op: *op,
                lhs: Box::new(self.expr(lhs, ctx)?),
                rhs: Box::new(self.expr(rhs, ctx)?),
                span: *span,
            }),
            ast::Expr::IsNull {
                operand,
                negated,
                span,
            } => Ok(BoundExpr::IsNull {
                operand: Box::new(self.expr(operand, ctx)?),
                negated: *negated,
                span: *span,
            }),
            ast::Expr::FunctionCall {
                name,
                distinct,
                args,
                star,
                span,
            } => self.function_call(name, *distinct, args, *star, *span, ctx),
            ast::Expr::Case {
                operand,
                whens,
                else_expr,
                span,
            } => {
                let operand = match operand {
                    Some(expr) => Some(Box::new(self.expr(expr, ctx)?)),
                    None => None,
                };
                let mut bound_whens = Vec::new();
                for (condition, value) in whens {
                    bound_whens.push((self.expr(condition, ctx)?, self.expr(value, ctx)?));
                }
                let else_expr = match else_expr {
                    Some(expr) => Some(Box::new(self.expr(expr, ctx)?)),
                    None => None,
                };
                Ok(BoundExpr::Case {
                    operand,
                    whens: bound_whens,
                    else_expr,
                    span: *span,
                })
            }
            ast::Expr::ListLiteral { items, span } => {
                let items: Result<Vec<_>, _> =
                    items.iter().map(|item| self.expr(item, ctx)).collect();
                Ok(BoundExpr::ListLiteral {
                    items: items?,
                    span: *span,
                })
            }
            ast::Expr::ListComprehension {
                variable,
                list,
                where_clause,
                map,
                span,
            } => {
                let list = Box::new(self.expr(list, ctx)?);
                // The comprehension variable shadows any outer binding
                // for the where/map sub-expressions.
                let shadowed = self.scope.get(variable).copied();
                let id = self.declare(variable, EntityKind::Value, vec![]);
                let where_clause = match where_clause {
                    Some(expr) => Some(Box::new(self.expr(expr, ctx)?)),
                    None => None,
                };
                let map = match map {
                    Some(expr) => Some(Box::new(self.expr(expr, ctx)?)),
                    None => None,
                };
                match shadowed {
                    Some(outer) => {
                        self.scope.insert(variable.clone(), outer);
                    }
                    None => {
                        self.scope.remove(variable);
                    }
                }
                Ok(BoundExpr::ListComprehension {
                    variable: id,
                    list,
                    where_clause,
                    map,
                    span: *span,
                })
            }
            ast::Expr::Quantifier {
                kind,
                variable,
                list,
                predicate,
                span,
            } => {
                let list = Box::new(self.expr(list, ctx)?);
                let shadowed = self.scope.get(variable).copied();
                let id = self.declare(variable, EntityKind::Value, vec![]);
                let predicate = Box::new(self.expr(predicate, ctx)?);
                self.restore(variable, shadowed);
                Ok(BoundExpr::Quantifier {
                    kind: *kind,
                    variable: id,
                    list,
                    predicate,
                    span: *span,
                })
            }
            ast::Expr::Reduce {
                accumulator,
                init,
                variable,
                list,
                expr,
                span,
            } => {
                let init = Box::new(self.expr(init, ctx)?);
                let list = Box::new(self.expr(list, ctx)?);
                // The accumulator and element variables scope the body.
                let shadowed_acc = self.scope.get(accumulator).copied();
                let acc_id = self.declare(accumulator, EntityKind::Value, vec![]);
                let shadowed_var = self.scope.get(variable).copied();
                let var_id = self.declare(variable, EntityKind::Value, vec![]);
                let body = Box::new(self.expr(expr, ctx)?);
                self.restore(variable, shadowed_var);
                self.restore(accumulator, shadowed_acc);
                Ok(BoundExpr::Reduce {
                    accumulator: acc_id,
                    init,
                    variable: var_id,
                    list,
                    expr: body,
                    span: *span,
                })
            }
            ast::Expr::MapLiteral { entries, span } => {
                let mut bound = Vec::new();
                for (key, value) in entries {
                    bound.push((key.clone(), self.expr(value, ctx)?));
                }
                Ok(BoundExpr::MapLiteral {
                    entries: bound,
                    span: *span,
                })
            }
            ast::Expr::Index { base, index, span } => Ok(BoundExpr::Index {
                base: Box::new(self.expr(base, ctx)?),
                index: Box::new(self.expr(index, ctx)?),
                span: *span,
            }),
            ast::Expr::Slice {
                base,
                from,
                to,
                span,
            } => Ok(BoundExpr::Slice {
                base: Box::new(self.expr(base, ctx)?),
                from: match from {
                    Some(expr) => Some(Box::new(self.expr(expr, ctx)?)),
                    None => None,
                },
                to: match to {
                    Some(expr) => Some(Box::new(self.expr(expr, ctx)?)),
                    None => None,
                },
                span: *span,
            }),
            ast::Expr::PatternPredicate { pattern, span } => {
                let bound = self.path_pattern(pattern, false)?;
                Ok(BoundExpr::PatternPredicate {
                    pattern: Box::new(bound),
                    span: *span,
                })
            }
        }
    }

    fn function_call(
        &mut self,
        name_segments: &[String],
        distinct: bool,
        args: &[ast::Expr],
        star: bool,
        span: Span,
        ctx: ExprCtx,
    ) -> Result<BoundExpr, BindError> {
        let name = name_segments.join(".");

        if let Some(def) = lookup_aggregate(&name) {
            if !ctx.aggregates_allowed {
                return Err(BindError::InvalidAggregation { span });
            }
            if ctx.in_aggregate {
                return Err(BindError::NestedAggregation { span });
            }
            if star {
                if def.name != "count" {
                    return Err(BindError::InvalidNumberOfArguments {
                        name,
                        expected: "1".into(),
                        got: 0,
                        span,
                    });
                }
                return Ok(BoundExpr::Aggregate {
                    def,
                    distinct,
                    arg: None,
                    span,
                });
            }
            if args.len() != 1 {
                return Err(BindError::InvalidNumberOfArguments {
                    name,
                    expected: "1".into(),
                    got: args.len(),
                    span,
                });
            }
            let inner = ExprCtx {
                aggregates_allowed: true,
                in_aggregate: true,
            };
            let arg = self.expr(&args[0], inner)?;
            return Ok(BoundExpr::Aggregate {
                def,
                distinct,
                arg: Some(Box::new(arg)),
                span,
            });
        }

        let Some(def) = lookup_function(&name) else {
            return Err(BindError::UnknownFunction { name, span });
        };
        if star || distinct {
            // `f(*)` and `f(DISTINCT x)` are aggregate-only forms.
            return Err(BindError::InvalidAggregation { span });
        }
        if args.len() < def.min_args || args.len() > def.max_args {
            return Err(BindError::InvalidNumberOfArguments {
                name,
                expected: if def.max_args == usize::MAX {
                    format!("at least {}", def.min_args)
                } else if def.min_args == def.max_args {
                    format!("{}", def.min_args)
                } else {
                    format!("{}..{}", def.min_args, def.max_args)
                },
                got: args.len(),
                span,
            });
        }
        let args: Result<Vec<_>, _> = args.iter().map(|arg| self.expr(arg, ctx)).collect();
        Ok(BoundExpr::Function {
            def,
            args: args?,
            span,
        })
    }
}

/// Does the bound expression contain an aggregate at any depth? Iterative
/// (explicit stack): the AST bound may be up to the parser's depth limit.
fn contains_aggregate(expr: &BoundExpr) -> bool {
    let mut stack = vec![expr];
    while let Some(expr) = stack.pop() {
        match expr {
            BoundExpr::Aggregate { .. } => return true,
            BoundExpr::Literal { .. }
            | BoundExpr::Parameter { .. }
            | BoundExpr::Variable { .. } => {}
            BoundExpr::Property { base, .. } => stack.push(base),
            BoundExpr::Unary { operand, .. } => stack.push(operand),
            BoundExpr::Binary { lhs, rhs, .. } => {
                stack.push(lhs);
                stack.push(rhs);
            }
            BoundExpr::IsNull { operand, .. } => stack.push(operand),
            BoundExpr::Function { args, .. } => stack.extend(args.iter()),
            BoundExpr::Case {
                operand,
                whens,
                else_expr,
                ..
            } => {
                stack.extend(operand.iter().map(|b| &**b));
                for (condition, value) in whens {
                    stack.push(condition);
                    stack.push(value);
                }
                stack.extend(else_expr.iter().map(|b| &**b));
            }
            BoundExpr::ListLiteral { items, .. } => stack.extend(items.iter()),
            BoundExpr::ListComprehension {
                list,
                where_clause,
                map,
                ..
            } => {
                stack.push(list);
                stack.extend(where_clause.iter().map(|b| &**b));
                stack.extend(map.iter().map(|b| &**b));
            }
            BoundExpr::Quantifier {
                list, predicate, ..
            } => {
                stack.push(list);
                stack.push(predicate);
            }
            BoundExpr::Reduce {
                init, list, expr, ..
            } => {
                stack.push(init);
                stack.push(list);
                stack.push(expr);
            }
            BoundExpr::MapLiteral { entries, .. } => {
                stack.extend(entries.iter().map(|(_, value)| value));
            }
            BoundExpr::Index { base, index, .. } => {
                stack.push(base);
                stack.push(index);
            }
            BoundExpr::Slice { base, from, to, .. } => {
                stack.push(base);
                stack.extend(from.iter().map(|b| &**b));
                stack.extend(to.iter().map(|b| &**b));
            }
            BoundExpr::PatternPredicate { pattern, .. } => {
                stack.extend(pattern.start.properties.iter());
                for (rel, node) in &pattern.steps {
                    stack.extend(rel.properties.iter());
                    stack.extend(node.properties.iter());
                }
            }
        }
    }
    false
}
