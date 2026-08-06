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
use crate::bind::error::{BindError, Suggestion};
use crate::span::Span;
use crate::suggest::nearest;

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
    bind_with(source, query, catalogue, mode, false)
}

/// As [`bind`], with relationship-type autodeclare (ADR-0060): in Strict
/// mode an unknown type in CREATE/MERGE position is recorded in
/// [`BoundQuery::autodeclared_rel_types`] instead of erroring. Match
/// positions still error — autodeclare never coins a type on read — except
/// for types this same query coins earlier in clause order.
pub fn bind_with(
    source: &str,
    query: &ast::Query,
    catalogue: &Catalogue,
    mode: BindMode,
    autodeclare: bool,
) -> Result<BoundQuery, BindError> {
    let mut binder = Binder {
        source,
        catalogue,
        mode,
        variables: Vec::new(),
        scope: HashMap::new(),
        undeclared_expr_labels: Vec::new(),
        autodeclare,
        autodeclared_rel_types: Vec::new(),
        undeclared_shape_properties: Vec::new(),
    };
    let mut clauses = Vec::new();
    for clause in &query.clauses {
        clauses.push(binder.clause(clause)?);
    }
    let mut undeclared_expr_labels = binder.undeclared_expr_labels;
    undeclared_expr_labels.sort();
    undeclared_expr_labels.dedup();
    let mut autodeclared_rel_types = binder.autodeclared_rel_types;
    autodeclared_rel_types.sort();
    autodeclared_rel_types.dedup();
    let mut undeclared_shape_properties = binder.undeclared_shape_properties;
    undeclared_shape_properties.sort();
    undeclared_shape_properties.dedup();
    Ok(BoundQuery {
        clauses,
        variables: binder.variables,
        undeclared_expr_labels,
        autodeclared_rel_types,
        undeclared_shape_properties,
    })
}

struct Binder<'a> {
    source: &'a str,
    catalogue: &'a Catalogue,
    mode: BindMode,
    variables: Vec<VarBinding>,
    /// Names currently visible.
    scope: HashMap<String, VarId>,
    /// Labels used in expression position (`n:Label`) that the non-empty
    /// catalogue does not declare. TCK semantics require these to
    /// evaluate false/null rather than error, so they surface as
    /// advisories — a typo'd label otherwise filters everything with no
    /// signal (acetone-2ck.3).
    undeclared_expr_labels: Vec<String>,
    autodeclare: bool,
    autodeclared_rel_types: Vec<String>,
    /// Property names in node-pattern map literals / SET targets that a
    /// types()-bearing label does not declare, with a did-you-mean
    /// suggestion. Open shape (ADR-0070): collected for advisories, never
    /// errors.
    undeclared_shape_properties: Vec<(String, String, Option<String>)>,
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

    /// The closest declared label to a mistyped one, for a "did you mean"
    /// hint. `None` when nothing is close.
    fn label_suggestion(&self, name: &str) -> Suggestion {
        Suggestion(nearest(name, self.catalogue.label_names()))
    }

    /// The closest declared relationship type to a mistyped one.
    fn rel_type_suggestion(&self, name: &str) -> Suggestion {
        Suggestion(nearest(name, self.catalogue.rel_type_names()))
    }

    /// The closest property declared by `label` to a mistyped one.
    fn property_suggestion(&self, label: &str, name: &str) -> Suggestion {
        Suggestion(nearest(
            name,
            self.catalogue.property_names(label).into_iter(),
        ))
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
                self.note_undeclared_set_property(target, key);
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
                        self.note_undeclared_set_property(target, property);
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
                                suggestion: self.label_suggestion(label),
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
                    self.note_undeclared_set_property(target, key);
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
        if rel.types.is_empty() {
            return Err(BindError::CreateRequiresRelType { span: rel.span });
        }
        if rel.types.len() != 1 {
            return Err(BindError::CreateRequiresSingleRelType { span: rel.span });
        }
        if self.mode == BindMode::Strict {
            for rel_type in &rel.types {
                if self.catalogue.rel_type(rel_type).is_none() {
                    if self.autodeclare {
                        // ADR-0060: coin it — the session appends the
                        // type to the schema in the write transaction.
                        if !self.autodeclared_rel_types.contains(rel_type) {
                            self.autodeclared_rel_types.push(rel_type.clone());
                        }
                        continue;
                    }
                    return Err(BindError::UnknownRelType {
                        name: rel_type.clone(),
                        declare_cmd: acetone_model::display::format_label(rel_type),
                        span: rel.span,
                        suggestion: self.rel_type_suggestion(rel_type),
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
        // Range hints (acetone-6g5.3.3): a WHERE conjunct comparing an
        // anchor variable's indexed property against a constant lets the
        // executor range-scan the index instead of label-scanning. The
        // WHERE still evaluates afterwards — the hint only prunes.
        if let Some(pred) = &where_clause {
            // Equality first: it is generally the more selective of the
            // two when a query offers both (acetone-7qw.9).
            attach_equality_hints(&mut patterns, pred, self.catalogue);
            attach_range_hints(&mut patterns, pred, self.catalogue);
        }
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
        // `YIELD *` expands to every declared column, in declared order.
        let requested: Vec<ast::YieldItem> = if c.yield_all {
            def.yields
                .iter()
                .map(|column| ast::YieldItem {
                    column: column.to_string(),
                    alias: None,
                })
                .collect()
        } else {
            c.yield_items.clone()
        };
        let mut yields: Vec<BoundYield> = Vec::new();
        for item in &requested {
            if !def.yields.contains(&item.column.as_str()) {
                return Err(BindError::UnknownYieldColumn {
                    procedure: name.clone(),
                    column: item.column.clone(),
                    span: c.span,
                });
            }
            // A yield binding cannot shadow a bound name (TCK Call1 [15])
            // nor repeat (Call5 [5][6]) — both VariableAlreadyBound in TCK
            // vocabulary. The checks apply to the *binding* name (the alias
            // when given), so `YIELD a AS b, b AS a` is legal.
            let bound_name = item.alias.clone().unwrap_or_else(|| item.column.clone());
            if self.scope.contains_key(bound_name.as_str())
                || yields.iter().any(|y| y.name == bound_name)
            {
                return Err(BindError::VariableAlreadyBound {
                    name: bound_name,
                    span: c.span,
                });
            }
            let id = self.declare(&bound_name, EntityKind::Value, vec![]);
            yields.push(BoundYield {
                column: item.column.clone(),
                name: bound_name,
                var: id,
            });
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

        // Families (a)/(d) of acetone-1qj, the TCK's grouping-key rule,
        // validated on the BOUND trees so parenthesisation, backticks,
        // whitespace and multi-line formatting are transparent (PR #244
        // review major 1) and `*` items — already expanded into
        // bound_items — contribute grouping keys (major 2): inside an
        // aggregate-containing item, a variable or property access is
        // legal only when it structurally matches a sibling non-aggregate
        // item (Return6 [18][19] valid; With6 [8] invalid — unprojected;
        // With6 [9] invalid — a COMPLEX projected expression cannot be
        // referenced, piecewise or verbatim, so only simple matches
        // count).
        if aggregating {
            let keys: Vec<&BoundExpr> = bound_items
                .iter()
                .filter(|(expr, _, _)| !contains_aggregate(expr))
                .map(|(expr, _, _)| expr)
                .collect();
            let probes: Vec<&BoundExpr> = bound_items
                .iter()
                .filter(|(expr, _, _)| contains_aggregate(expr))
                .map(|(expr, _, _)| expr)
                .collect();
            let index = KeyIndex::build(&keys, &probes);
            for (expr, _, span) in &bound_items {
                if contains_aggregate(expr) {
                    validate_grouping_refs(
                        expr,
                        &index,
                        &[],
                        RefErrorMode::Ambiguous,
                        *span,
                        &mut Vec::new(),
                        &self.variables,
                    )?;
                }
            }
        }

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
        // bound before but not after WITH" is valid); the scope narrows to
        // the projected names afterwards. The union is REFERENCE
        // resolution only — after DISTINCT or aggregation the grouping-key
        // rule below constrains what a resolved reference may actually be
        // (acetone-1qj; formerly a conceded over-accept, now enforced).
        // WITH … WHERE remains unvalidated against the grouping rule — a
        // recorded residual (PR #244 review finding 7.1, bead filed).
        let union_scope: HashMap<String, VarId> = self
            .scope
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .chain(new_scope.iter().map(|(k, v)| (k.clone(), *v)))
            .collect();
        // ORDER BY binds in the UNION scope — pre-projection names must
        // RESOLVE even after aggregation/DISTINCT, because an ordering
        // expression may reference them exactly as far as it reduces to
        // projected grouping keys, aliases and constants (acetone-1qj; TCK
        // WithOrderBy2 [22]-[24]: `WITH a.name AS name, count(*) … ORDER
        // BY a.name` is valid). Aggregates in ORDER BY are only meaningful
        // when the projection aggregates (ReturnOrderBy2 [14] —
        // InvalidAggregation); the reference rule is validated on the
        // bound trees after binding.
        self.scope = union_scope;
        let order_ctx = if aggregating { AGG_OK } else { NO_AGG };
        let order_by_result: Result<Vec<(BoundExpr, bool)>, BindError> = p
            .order_by
            .iter()
            .map(|sort| Ok((self.expr(&sort.expr, order_ctx)?, sort.descending)))
            .collect();
        let order_by = order_by_result?;
        if p.distinct || aggregating {
            // Reference targets: every item for DISTINCT (each projected
            // expression is a dedup key); non-aggregate items when
            // aggregating. Aliases resolve to output VarIds because the
            // union scope lets new names win, so the walk accepts them by
            // id. NOTE (PR #244 review finding 7): ORDER BY permits a
            // WHOLE match on a complex projected expression — needed for
            // the DISTINCT case — which will admit WithOrderBy4 [20] /
            // ReturnOrderBy6 [5] (TCK: AmbiguousAggregationExpression)
            // once aggregate-outside-projection execution lands; those
            // scenarios are held Unsupported today by that limitation
            // alone (bead filed at review).
            let keys: Vec<&BoundExpr> = items
                .iter()
                .filter(|item| !aggregating || !contains_aggregate(&item.expr))
                .map(|item| &item.expr)
                .collect();
            let probes: Vec<&BoundExpr> = order_by.iter().map(|(bound, _)| bound).collect();
            let index = KeyIndex::build(&keys, &probes);
            let output_ids: Vec<u32> = new_scope.values().map(|v| v.0).collect();
            for (bound, _) in &order_by {
                validate_grouping_refs(
                    bound,
                    &index,
                    &output_ids,
                    RefErrorMode::Undefined,
                    p.span,
                    &mut Vec::new(),
                    &self.variables,
                )?;
            }
        }
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
                        suggestion: self.label_suggestion(label),
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
                self.note_undeclared_properties(&node.labels, expr);
                Some(self.expr(expr, NO_AGG)?)
            }
            None => None,
        };
        let index_hint = self.index_hint(node);
        let mint_surrogate = node.labels.iter().any(|l| {
            self.catalogue
                .label(l)
                .is_some_and(|def| def.is_surrogate())
        });
        Ok(BoundNodePattern {
            var,
            labels: node.labels.clone(),
            properties,
            index_hints: index_hint.into_iter().collect(),
            mint_surrogate,
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
                if self.catalogue.rel_type(rel_type).is_none()
                    // A type this query already coined (clause order)
                    // reads as declared; a MATCH of a genuinely unknown
                    // type stays an error — autodeclare never coins on
                    // read (ADR-0060).
                    && !self.autodeclared_rel_types.contains(rel_type)
                {
                    return Err(BindError::UnknownRelType {
                        name: rel_type.clone(),
                        declare_cmd: acetone_model::display::format_label(rel_type),
                        span: rel.span,
                        suggestion: self.rel_type_suggestion(rel_type),
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

    /// Strict mode, open shape (ADR-0070): a property name a
    /// types()-bearing label does not declare is COLLECTED for a typo
    /// advisory — never an error. Declared types constrain the properties
    /// they name (spec §2, "optional for shape"); the closure this method
    /// once enforced was an accident of reachability and punished
    /// incremental typing (acetone-7qw.17).
    ///
    /// Property ACCESS expressions (`n.zzz` in WHERE/RETURN) remain
    /// unexamined — openCypher property access on a missing property
    /// yields null; that lint belongs elsewhere (acetone-1qj).
    fn note_undeclared_properties(&mut self, labels: &[String], properties: &ast::Expr) {
        if self.mode != BindMode::Strict {
            return;
        }
        let ast::Expr::MapLiteral { entries, span: _ } = properties else {
            return; // parameter property maps carry no static names
        };
        for (property, _) in entries {
            self.note_undeclared_property(labels, property);
        }
    }

    /// The membership test behind the advisory, for one property against a
    /// pattern's labels as a WHOLE: declared anywhere on ANY of them — key,
    /// type, `--require`, UNIQUE, or index (PR #241 review majors 1/8: a
    /// property the schema mandates is not a typo, and a property declared
    /// on a sibling label of the same pattern is not one either). Advises
    /// only when at least one label carries declared types, attributed to
    /// the first such label.
    fn note_undeclared_property(&mut self, labels: &[String], property: &str) {
        if labels
            .iter()
            .any(|label| self.catalogue.declares_property(label, property))
        {
            return;
        }
        let Some(shaped) = labels.iter().find(|label| {
            self.catalogue
                .label(label)
                .is_some_and(|def| !def.types().is_empty())
        }) else {
            return; // no label with declared types: nothing to compare against
        };
        let suggestion = self.property_suggestion(shaped, property).0;
        self.undeclared_shape_properties
            .push((shaped.clone(), property.to_string(), suggestion));
    }

    /// The `SET`/`REMOVE` counterpart of
    /// [`Binder::note_undeclared_properties`]: the same open-shape advisory
    /// for a single property name against the target variable's
    /// statically-known labels (ADR-0070 — map literals, SET and REMOVE
    /// behave identically).
    fn note_undeclared_set_property(&mut self, target: VarId, property: &str) {
        if self.mode != BindMode::Strict {
            return;
        }
        let labels = self.variables[target.0 as usize].labels.clone();
        self.note_undeclared_property(&labels, property);
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
        // KeySeek only when EVERY key property is pinned: a partial
        // prefix cannot point-look-up, and hinting it would displace a
        // usable IndexSeek/IndexRange (PR #206 review finding 6).
        if let Some(def) = self.catalogue.label(label) {
            let key = def.key();
            if !key.is_empty() && key.iter().all(|k| pinned.iter().any(|p| p == k)) {
                return Some(IndexHint::KeySeek {
                    label: label.clone(),
                    key: key.to_vec(),
                    values: None,
                });
            }
        }
        if let Some((name, def)) = self.catalogue.seek_index_on(label, &pinned) {
            return Some(IndexHint::IndexSeek {
                name: name.to_string(),
                label: label.clone(),
                properties: def.properties().to_vec(),
                values: None,
            });
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
            ast::Expr::HasLabels {
                subject,
                labels,
                span,
            } => {
                if self.mode == BindMode::Strict && !self.catalogue.is_empty() {
                    for label in labels {
                        // `x:NAME` on a relationship subject is a rel-type
                        // test at eval time, so a declared rel type is not
                        // suspicious in this position.
                        if self.catalogue.label(label).is_none()
                            && self.catalogue.rel_type(label).is_none()
                        {
                            self.undeclared_expr_labels.push(label.clone());
                        }
                    }
                }
                Ok(BoundExpr::HasLabels {
                    subject: Box::new(self.expr(subject, ctx)?),
                    labels: labels.clone(),
                    span: *span,
                })
            }
            ast::Expr::PatternComprehension {
                pattern,
                where_clause,
                map,
                span,
            } => {
                // Fresh pattern variables (including the path variable)
                // are visible to the WHERE and map expressions only;
                // outer bindings referenced by name anchor the pattern.
                // Restore the whole scope afterwards so nothing leaks.
                // Aggregates are never valid inside the comprehension —
                // the map runs once per match, outside any grouping —
                // so both sub-expressions bind NO_AGG regardless of the
                // surrounding context (openCypher InvalidAggregation).
                let saved_scope = self.scope.clone();
                let bound_pattern = self.path_pattern(pattern, true)?;
                let bound_where = match where_clause {
                    Some(expr) => Some(Box::new(self.expr(expr, NO_AGG)?)),
                    None => None,
                };
                let bound_map = Box::new(self.expr(map, NO_AGG)?);
                self.scope = saved_scope;
                Ok(BoundExpr::PatternComprehension {
                    pattern: Box::new(bound_pattern),
                    where_clause: bound_where,
                    map: bound_map,
                    span: *span,
                })
            }
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
                // The where/map bodies run per element, outside any
                // grouping, so an aggregate inside them is openCypher
                // InvalidAggregation (acetone-2ck.7) — the list operand
                // above stays in the surrounding context (aggregating
                // over the group INTO the list is legal).
                let where_clause = match where_clause {
                    Some(expr) => Some(Box::new(self.expr(expr, NO_AGG)?)),
                    None => None,
                };
                let map = match map {
                    Some(expr) => Some(Box::new(self.expr(expr, NO_AGG)?)),
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
                // Per-element predicate: aggregates are
                // InvalidAggregation here (acetone-2ck.7).
                let predicate = Box::new(self.expr(predicate, NO_AGG)?);
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
                // Per-element body: aggregates are InvalidAggregation
                // here (acetone-2ck.7); init and list stay in the
                // surrounding context.
                let body = Box::new(self.expr(expr, NO_AGG)?);
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
            let suggestion = Suggestion(nearest(&name, function_names()));
            return Err(BindError::UnknownFunction {
                name,
                span,
                suggestion,
            });
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
/// Attach `IndexRange` hints (acetone-6g5.3.3): for each anchor pattern
/// with a fresh single-labelled variable and no stronger hint, a WHERE
/// conjunct `var.prop </<=/>/>= const` (either orientation) over a
/// declared index on `(label, prop)` becomes a range hint. Bounds are
/// constant-ish only (literals, parameters); the predicate itself still
/// evaluates after matching, so the hint can only prune, never widen.
/// Attach an equality seek from a `WHERE` clause (acetone-7qw.9).
///
/// Only pattern property maps used to pin an index, so `MATCH (n:H {b: 3})`
/// used the index while `MATCH (n:H) WHERE n.b = 3` — the form most people
/// write — scanned. Ranges in `WHERE` already attached hints; equality did
/// not. Runs BEFORE the range pass, since an equality is generally the more
/// selective of the two when both are available.
fn attach_equality_hints(
    patterns: &mut [crate::bind::bound::BoundPathPattern],
    pred: &BoundExpr,
    catalogue: &Catalogue,
) {
    use std::collections::BTreeMap;
    let mut pins: BTreeMap<(u32, String), RangeBound> = BTreeMap::new();
    collect_equality_pins(pred, &mut pins);
    if pins.is_empty() {
        return;
    }
    for pattern in patterns {
        let start = &mut pattern.start;
        let Some(var) = start.var else { continue };
        let [label] = start.labels.as_slice() else {
            continue;
        };
        let pinned: Vec<&str> = pins
            .keys()
            .filter(|(v, _)| *v == var.0)
            .map(|(_, p)| p.as_str())
            .collect();
        if pinned.is_empty() {
            continue;
        }
        let value_of = |property: &str| pins.get(&(var.0, property.to_owned())).cloned();
        // `MATCH (n:H {b: 0}) WHERE n.b = 0` spells one predicate twice.
        // Without this, both spellings attach a hint on the same target and
        // an unselective probe walks the index to the cap twice before
        // declining (PR #224 review nit 6). The inline pin is already
        // attached by the time this pass runs, so first-wins is the inline
        // one — which is the same seek whenever the two spellings agree, and
        // when they contradict each other neither can match anyway.
        //
        // Labels and index names live in separate namespaces, so they are
        // tested separately: comparing one against the other let an index
        // sharing a label's name suppress a legitimate `KeySeek` (PR #224
        // review nit 4).
        let keyseek_on = |hints: &[IndexHint], label: &str| {
            hints
                .iter()
                .any(|h| matches!(h, IndexHint::KeySeek { label: l, .. } if l == label))
        };
        let index_seek_on = |hints: &[IndexHint], name: &str| {
            hints.iter().any(|h| {
                matches!(
                    h,
                    IndexHint::IndexSeek { name: n, .. } | IndexHint::IndexRange { name: n, .. }
                        if n == name
                )
            })
        };
        // KeySeek only when EVERY key property is pinned, mirroring the
        // pattern-map path's rule.
        if let Some(def) = catalogue.label(label) {
            let key = def.key();
            if !key.is_empty() && key.iter().all(|k| pinned.iter().any(|p| p == k)) {
                let values: Option<Vec<RangeBound>> = key.iter().map(|k| value_of(k)).collect();
                if let Some(values) = values {
                    if !keyseek_on(&start.index_hints, label) {
                        start.index_hints.push(IndexHint::KeySeek {
                            label: label.clone(),
                            key: key.to_vec(),
                            values: Some(values),
                        });
                    }
                    continue;
                }
            }
        }
        if let Some((name, def)) = catalogue.seek_index_on(label, &pinned) {
            let properties = def.properties().to_vec();
            let values: Option<Vec<RangeBound>> = properties.iter().map(|p| value_of(p)).collect();
            if let Some(values) = values
                && !index_seek_on(&start.index_hints, name)
            {
                start.index_hints.push(IndexHint::IndexSeek {
                    name: name.to_string(),
                    label: label.clone(),
                    properties,
                    values: Some(values),
                });
            }
        }
    }
}

/// Walk a WHERE's AND-conjuncts collecting `var.prop = const` pins (either
/// operand order).
fn collect_equality_pins(
    expr: &BoundExpr,
    out: &mut std::collections::BTreeMap<(u32, String), RangeBound>,
) {
    let as_prop = |e: &BoundExpr| -> Option<(u32, String)> {
        let BoundExpr::Property { base, key, .. } = e else {
            return None;
        };
        let BoundExpr::Variable { id, .. } = &**base else {
            return None;
        };
        Some((id.0, key.clone()))
    };
    let as_const = |e: &BoundExpr| -> Option<RangeBound> {
        match e {
            BoundExpr::Literal { value, .. } => Some(RangeBound::Literal(value.clone())),
            BoundExpr::Parameter { name, .. } => Some(RangeBound::Parameter(name.clone())),
            _ => None,
        }
    };
    if let BoundExpr::Binary { op, lhs, rhs, .. } = expr {
        match op {
            crate::ast::BinaryOp::And => {
                collect_equality_pins(lhs, out);
                collect_equality_pins(rhs, out);
            }
            crate::ast::BinaryOp::Eq => {
                if let (Some(prop), Some(value)) = (as_prop(lhs), as_const(rhs)) {
                    out.insert(prop, value);
                } else if let (Some(prop), Some(value)) = (as_prop(rhs), as_const(lhs)) {
                    out.insert(prop, value);
                }
            }
            _ => {}
        }
    }
}

/// Attach an `IndexRange` candidate from a `WHERE` clause's range
/// predicates on the anchor variable. Appends rather than replaces: hints
/// are ordered candidates, so a declining equality falls through to this
/// (ADR-0065).
fn attach_range_hints(
    patterns: &mut [crate::bind::bound::BoundPathPattern],
    pred: &BoundExpr,
    catalogue: &Catalogue,
) {
    use std::collections::BTreeMap;
    let mut bounds: RangeBounds = BTreeMap::new();
    collect_range_bounds(pred, &mut bounds);
    if bounds.is_empty() {
        return;
    }
    for pattern in patterns {
        let start = &mut pattern.start;
        let Some(var) = start.var else { continue };
        let [label] = start.labels.as_slice() else {
            continue;
        };
        for ((bound_var, property), (lower, upper)) in &bounds {
            if *bound_var != var.0 || (lower.is_none() && upper.is_none()) {
                continue;
            }
            if let Some((name, _)) = catalogue.index_on(label.as_str(), property) {
                start.index_hints.push(IndexHint::IndexRange {
                    name: name.to_string(),
                    label: label.to_string(),
                    property: property.clone(),
                    lower: lower.clone(),
                    upper: upper.clone(),
                });
                break;
            }
        }
    }
}

/// Per-(variable, property) collected bounds: (lower, upper).
type RangeBounds = std::collections::BTreeMap<
    (u32, String),
    (Option<(RangeBound, bool)>, Option<(RangeBound, bool)>),
>;

/// Walk a WHERE's AND-conjuncts collecting `var.prop op const` bounds.
fn collect_range_bounds(expr: &BoundExpr, out: &mut RangeBounds) {
    let as_prop = |e: &BoundExpr| -> Option<(u32, String)> {
        let BoundExpr::Property { base, key, .. } = e else {
            return None;
        };
        let BoundExpr::Variable { id, .. } = &**base else {
            return None;
        };
        Some((id.0, key.clone()))
    };
    let as_const = |e: &BoundExpr| -> Option<RangeBound> {
        match e {
            BoundExpr::Literal { value, .. } => Some(RangeBound::Literal(value.clone())),
            BoundExpr::Parameter { name, .. } => Some(RangeBound::Parameter(name.clone())),
            _ => None,
        }
    };
    match expr {
        BoundExpr::Binary {
            op: ast::BinaryOp::And,
            lhs,
            rhs,
            ..
        } => {
            collect_range_bounds(lhs, out);
            collect_range_bounds(rhs, out);
        }
        BoundExpr::Binary { op, lhs, rhs, .. } => {
            // (is_lower_bound_for_prop, inclusive) per op with the
            // property on the left; the mirrored orientation flips it.
            let shape = match op {
                ast::BinaryOp::Gt => Some((true, false)),
                ast::BinaryOp::Ge => Some((true, true)),
                ast::BinaryOp::Lt => Some((false, false)),
                ast::BinaryOp::Le => Some((false, true)),
                _ => None,
            };
            let Some((lower_side, inclusive)) = shape else {
                return;
            };
            let (target, bound, is_lower) =
                if let (Some(p), Some(c)) = (as_prop(lhs), as_const(rhs)) {
                    (p, c, lower_side)
                } else if let (Some(c), Some(p)) = (as_const(lhs), as_prop(rhs)) {
                    (p, c, !lower_side)
                } else {
                    return;
                };
            let entry = out.entry(target).or_default();
            let slot = if is_lower { &mut entry.0 } else { &mut entry.1 };
            // First bound wins; further conjuncts still filter at WHERE
            // evaluation, so ignoring them is correct, just unpruned.
            if slot.is_none() {
                *slot = Some((bound, inclusive));
            }
        }
        _ => {}
    }
}

/// A span-insensitive structural digest matching [`same_bound`]'s
/// equality classes (Phase 10 security review MAJOR 2): equal-by-
/// `same_bound` trees get equal digests — including alpha-equivalent
/// iteration constructs, whose locals hash by stack position exactly as
/// the correspondence stack pairs them — and spans never contribute.
/// Patterns digest by node address (conservatively unique), mirroring
/// `same_bound`'s conservative-unequal arm. Hashing is `DefaultHasher`
/// (SipHash, randomly keyed per process), so bucket collisions are not
/// craftable; a digest match is always CONFIRMED with `same_bound`.
///
/// One post-order pass digests every node of a tree in O(size) —
/// `digest_tree` records per-node digests keyed by node address — so the
/// grouping-key whole-match probe is an O(1) average lookup instead of a
/// structural comparison against every key at every node (which measured
/// quadratic in query size, outside the wall clock: binding runs before
/// the Governor exists).
fn digest_tree(expr: &BoundExpr, locals: &mut Vec<u32>, out: &mut HashMap<usize, u64>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let addr = expr as *const BoundExpr as usize;
    match expr {
        BoundExpr::Literal { value, .. } => {
            1u8.hash(&mut h);
            // Debug distinguishes 0.0 from -0.0 while same_bound's `==`
            // does not — normalise so the digest relation stays a
            // refinement of equality (PR #254 review nit).
            match value {
                crate::ast::Literal::Float(f) if *f == 0.0 => {
                    "Float(0.0)".hash(&mut h);
                }
                other => format!("{other:?}").hash(&mut h),
            }
        }
        BoundExpr::Parameter { name, .. } => {
            2u8.hash(&mut h);
            name.hash(&mut h);
        }
        BoundExpr::Variable { id, .. } => {
            // A locally-bound variable hashes by its stack position (the
            // alpha-equivalence class); a free one by its VarId.
            match locals.iter().rev().position(|l| *l == id.0) {
                Some(depth) => {
                    3u8.hash(&mut h);
                    depth.hash(&mut h);
                }
                None => {
                    4u8.hash(&mut h);
                    id.0.hash(&mut h);
                }
            }
        }
        BoundExpr::Property { base, key, .. } => {
            5u8.hash(&mut h);
            key.hash(&mut h);
            digest_tree(base, locals, out).hash(&mut h);
        }
        BoundExpr::Unary { op, operand, .. } => {
            6u8.hash(&mut h);
            format!("{op:?}").hash(&mut h);
            digest_tree(operand, locals, out).hash(&mut h);
        }
        BoundExpr::Binary { op, lhs, rhs, .. } => {
            7u8.hash(&mut h);
            format!("{op:?}").hash(&mut h);
            digest_tree(lhs, locals, out).hash(&mut h);
            digest_tree(rhs, locals, out).hash(&mut h);
        }
        BoundExpr::IsNull {
            operand, negated, ..
        } => {
            8u8.hash(&mut h);
            negated.hash(&mut h);
            digest_tree(operand, locals, out).hash(&mut h);
        }
        BoundExpr::HasLabels {
            subject, labels, ..
        } => {
            9u8.hash(&mut h);
            labels.hash(&mut h);
            digest_tree(subject, locals, out).hash(&mut h);
        }
        BoundExpr::Function { def, args, .. } => {
            10u8.hash(&mut h);
            def.name.hash(&mut h);
            for arg in args {
                digest_tree(arg, locals, out).hash(&mut h);
            }
        }
        BoundExpr::Aggregate {
            def, distinct, arg, ..
        } => {
            11u8.hash(&mut h);
            def.name.hash(&mut h);
            distinct.hash(&mut h);
            if let Some(arg) = arg {
                digest_tree(arg, locals, out).hash(&mut h);
            }
        }
        BoundExpr::Case {
            operand,
            whens,
            else_expr,
            ..
        } => {
            12u8.hash(&mut h);
            if let Some(operand) = operand {
                digest_tree(operand, locals, out).hash(&mut h);
            }
            for (condition, value) in whens {
                digest_tree(condition, locals, out).hash(&mut h);
                digest_tree(value, locals, out).hash(&mut h);
            }
            if let Some(else_expr) = else_expr {
                digest_tree(else_expr, locals, out).hash(&mut h);
            }
        }
        BoundExpr::ListLiteral { items, .. } => {
            13u8.hash(&mut h);
            for item in items {
                digest_tree(item, locals, out).hash(&mut h);
            }
        }
        BoundExpr::MapLiteral { entries, .. } => {
            14u8.hash(&mut h);
            for (key, value) in entries {
                key.hash(&mut h);
                digest_tree(value, locals, out).hash(&mut h);
            }
        }
        BoundExpr::Index { base, index, .. } => {
            15u8.hash(&mut h);
            digest_tree(base, locals, out).hash(&mut h);
            digest_tree(index, locals, out).hash(&mut h);
        }
        BoundExpr::Slice { base, from, to, .. } => {
            16u8.hash(&mut h);
            digest_tree(base, locals, out).hash(&mut h);
            if let Some(from) = from {
                digest_tree(from, locals, out).hash(&mut h);
            }
            if let Some(to) = to {
                digest_tree(to, locals, out).hash(&mut h);
            }
        }
        BoundExpr::ListComprehension {
            variable,
            list,
            where_clause,
            map,
            ..
        } => {
            17u8.hash(&mut h);
            digest_tree(list, locals, out).hash(&mut h);
            locals.push(variable.0);
            if let Some(where_clause) = where_clause {
                digest_tree(where_clause, locals, out).hash(&mut h);
            }
            if let Some(map) = map {
                digest_tree(map, locals, out).hash(&mut h);
            }
            locals.pop();
        }
        BoundExpr::Quantifier {
            kind,
            variable,
            list,
            predicate,
            ..
        } => {
            18u8.hash(&mut h);
            format!("{kind:?}").hash(&mut h);
            digest_tree(list, locals, out).hash(&mut h);
            locals.push(variable.0);
            digest_tree(predicate, locals, out).hash(&mut h);
            locals.pop();
        }
        BoundExpr::Reduce {
            accumulator,
            init,
            variable,
            list,
            expr: body,
            ..
        } => {
            19u8.hash(&mut h);
            digest_tree(init, locals, out).hash(&mut h);
            digest_tree(list, locals, out).hash(&mut h);
            locals.push(accumulator.0);
            locals.push(variable.0);
            digest_tree(body, locals, out).hash(&mut h);
            locals.pop();
            locals.pop();
        }
        BoundExpr::PatternComprehension {
            pattern,
            where_clause,
            map,
            ..
        } => {
            // The pattern NODE digests by address — conservatively unique,
            // mirroring same_bound's always-unequal arm (two pattern nodes
            // never whole-match). But its CHILDREN must still be digested,
            // under exactly the locals discipline validate_grouping_refs
            // uses, because the walk recurses into them and runs
            // whole_match at every node — a missing digest is a false
            // NEGATIVE that silently narrows what binds (PR #254 review
            // blocker: free captures inside pattern bodies stopped
            // whole-matching their grouping keys).
            20u8.hash(&mut h);
            addr.hash(&mut h);
            let depth = locals.len();
            locals.extend(pattern.start.var.iter().map(|v| v.0));
            for (rel, node) in &pattern.steps {
                locals.extend(rel.var.iter().map(|v| v.0));
                locals.extend(node.var.iter().map(|v| v.0));
            }
            for e in
                pattern
                    .start
                    .properties
                    .iter()
                    .chain(pattern.steps.iter().flat_map(|(rel, node)| {
                        rel.properties.iter().chain(node.properties.iter())
                    }))
                    .chain(where_clause.as_deref())
                    .chain(std::iter::once(&**map))
            {
                digest_tree(e, locals, out);
            }
            locals.truncate(depth);
        }
        BoundExpr::PatternPredicate { pattern, .. } => {
            // Same shape; property maps only, mirroring
            // push_bound_children (pattern predicates introduce no
            // variables — binder-enforced — so property maps reference the
            // row and digest under the OUTER locals).
            21u8.hash(&mut h);
            addr.hash(&mut h);
            for e in pattern.start.properties.iter().chain(
                pattern
                    .steps
                    .iter()
                    .flat_map(|(rel, node)| rel.properties.iter().chain(node.properties.iter())),
            ) {
                digest_tree(e, locals, out);
            }
        }
    }
    let digest = h.finish();
    out.insert(addr, digest);
    digest
}

/// Span-insensitive structural equality over bound expressions — the
/// grouping-key match relation (acetone-1qj, PR #244 review major 1):
/// bound trees are already free of parenthesisation, backtick and
/// whitespace variance, and `VarId` equality is meaningful because
/// projection items bind in the pre-scope the ORDER BY union shares.
/// Iteration constructs compare via a local-correspondence stack — their
/// locally-declared `VarId`s differ per binding site, so two ids are also
/// equal when they are a corresponding local pair (PR #244 re-review:
/// without this, `ORDER BY` repeating a projected comprehension that
/// captures a free outer variable was rejected). Patterns remain
/// conservatively unequal — safe for the pattern's OWN variables (the
/// validation walk pushes them as locals) but not for other free
/// variables captured in the body, so ORDER BY repeating such a pattern
/// comprehension verbatim over-rejects (recorded in acetone-7qw.24).
fn same_bound(a: &BoundExpr, b: &BoundExpr) -> bool {
    same_bound_in(a, b, &mut Vec::new())
}

fn same_bound_in(a: &BoundExpr, b: &BoundExpr, pairs: &mut Vec<(u32, u32)>) -> bool {
    use BoundExpr as E;
    match (a, b) {
        (E::Literal { value: a, .. }, E::Literal { value: b, .. }) => a == b,
        (E::Parameter { name: a, .. }, E::Parameter { name: b, .. }) => a == b,
        (E::Variable { id: a, .. }, E::Variable { id: b, .. }) => {
            a == b || pairs.iter().rev().any(|(x, y)| (x, y) == (&a.0, &b.0))
        }
        (
            E::Property {
                base: ab, key: ak, ..
            },
            E::Property {
                base: bb, key: bk, ..
            },
        ) => ak == bk && same_bound_in(ab, bb, pairs),
        (
            E::Unary {
                op: ao,
                operand: aa,
                ..
            },
            E::Unary {
                op: bo,
                operand: ba,
                ..
            },
        ) => ao == bo && same_bound_in(aa, ba, pairs),
        (
            E::Binary {
                op: ao,
                lhs: al,
                rhs: ar,
                ..
            },
            E::Binary {
                op: bo,
                lhs: bl,
                rhs: br,
                ..
            },
        ) => ao == bo && same_bound_in(al, bl, pairs) && same_bound_in(ar, br, pairs),
        (
            E::IsNull {
                operand: aa,
                negated: an,
                ..
            },
            E::IsNull {
                operand: ba,
                negated: bn,
                ..
            },
        ) => an == bn && same_bound_in(aa, ba, pairs),
        (
            E::HasLabels {
                subject: aa,
                labels: al,
                ..
            },
            E::HasLabels {
                subject: ba,
                labels: bl,
                ..
            },
        ) => al == bl && same_bound_in(aa, ba, pairs),
        (
            E::Function {
                def: ad, args: aa, ..
            },
            E::Function {
                def: bd, args: ba, ..
            },
        ) => {
            ad.name == bd.name
                && aa.len() == ba.len()
                && aa.iter().zip(ba).all(|(x, y)| same_bound_in(x, y, pairs))
        }
        (
            E::Aggregate {
                def: ad,
                distinct: adi,
                arg: aa,
                ..
            },
            E::Aggregate {
                def: bd,
                distinct: bdi,
                arg: ba,
                ..
            },
        ) => {
            ad.name == bd.name
                && adi == bdi
                && match (aa, ba) {
                    (None, None) => true,
                    (Some(x), Some(y)) => same_bound_in(x, y, pairs),
                    _ => false,
                }
        }
        (
            E::Case {
                operand: ao,
                whens: aw,
                else_expr: ae,
                ..
            },
            E::Case {
                operand: bo,
                whens: bw,
                else_expr: be,
                ..
            },
        ) => {
            let opt = |x: &Option<Box<BoundExpr>>,
                       y: &Option<Box<BoundExpr>>,
                       pairs: &mut Vec<(u32, u32)>| match (x, y) {
                (None, None) => true,
                (Some(x), Some(y)) => same_bound_in(x, y, pairs),
                _ => false,
            };
            opt(ao, bo, pairs)
                && opt(ae, be, pairs)
                && aw.len() == bw.len()
                && aw.iter().zip(bw).all(|((ac, av), (bc, bv))| {
                    same_bound_in(ac, bc, pairs) && same_bound_in(av, bv, pairs)
                })
        }
        (E::ListLiteral { items: aa, .. }, E::ListLiteral { items: ba, .. }) => {
            aa.len() == ba.len() && aa.iter().zip(ba).all(|(x, y)| same_bound_in(x, y, pairs))
        }
        (E::MapLiteral { entries: aa, .. }, E::MapLiteral { entries: ba, .. }) => {
            aa.len() == ba.len()
                && aa
                    .iter()
                    .zip(ba)
                    .all(|((ak, av), (bk, bv))| ak == bk && same_bound_in(av, bv, pairs))
        }
        (
            E::Index {
                base: ab,
                index: ai,
                ..
            },
            E::Index {
                base: bb,
                index: bi,
                ..
            },
        ) => same_bound_in(ab, bb, pairs) && same_bound_in(ai, bi, pairs),
        (
            E::Slice {
                base: ab,
                from: af,
                to: at,
                ..
            },
            E::Slice {
                base: bb,
                from: bf,
                to: bt,
                ..
            },
        ) => {
            let opt = |x: &Option<Box<BoundExpr>>,
                       y: &Option<Box<BoundExpr>>,
                       pairs: &mut Vec<(u32, u32)>| match (x, y) {
                (None, None) => true,
                (Some(x), Some(y)) => same_bound_in(x, y, pairs),
                _ => false,
            };
            same_bound_in(ab, bb, pairs) && opt(af, bf, pairs) && opt(at, bt, pairs)
        }
        (
            E::ListComprehension {
                variable: av,
                list: al,
                where_clause: aw,
                map: am,
                ..
            },
            E::ListComprehension {
                variable: bv,
                list: bl,
                where_clause: bw,
                map: bm,
                ..
            },
        ) => {
            if !same_bound_in(al, bl, pairs) {
                return false;
            }
            pairs.push((av.0, bv.0));
            let opt = |x: &Option<Box<BoundExpr>>,
                       y: &Option<Box<BoundExpr>>,
                       pairs: &mut Vec<(u32, u32)>| match (x, y) {
                (None, None) => true,
                (Some(x), Some(y)) => same_bound_in(x, y, pairs),
                _ => false,
            };
            let eq = opt(aw, bw, pairs) && opt(am, bm, pairs);
            pairs.pop();
            eq
        }
        (
            E::Quantifier {
                kind: ak,
                variable: av,
                list: al,
                predicate: ap,
                ..
            },
            E::Quantifier {
                kind: bk,
                variable: bv,
                list: bl,
                predicate: bp,
                ..
            },
        ) => {
            if ak != bk || !same_bound_in(al, bl, pairs) {
                return false;
            }
            pairs.push((av.0, bv.0));
            let eq = same_bound_in(ap, bp, pairs);
            pairs.pop();
            eq
        }
        (
            E::Reduce {
                accumulator: aa,
                init: ai,
                variable: av,
                list: al,
                expr: ae,
                ..
            },
            E::Reduce {
                accumulator: ba,
                init: bi,
                variable: bv,
                list: bl,
                expr: be,
                ..
            },
        ) => {
            if !same_bound_in(ai, bi, pairs) || !same_bound_in(al, bl, pairs) {
                return false;
            }
            pairs.push((aa.0, ba.0));
            pairs.push((av.0, bv.0));
            let eq = same_bound_in(ae, be, pairs);
            pairs.pop();
            pairs.pop();
            eq
        }
        // Patterns: conservatively unequal — see the doc comment.
        _ => false,
    }
}

/// Push every direct child of a bound expression — the shared traversal
/// behind [`contains_aggregate`] and the grouping-reference walk's
/// catch-all arm. Aggregate arguments ARE pushed; callers that must not
/// descend into aggregates handle that variant before consulting this.
fn push_bound_children<'e>(expr: &'e BoundExpr, out: &mut Vec<&'e BoundExpr>) {
    match expr {
        BoundExpr::Aggregate { arg, .. } => out.extend(arg.iter().map(|b| &**b)),
        BoundExpr::Literal { .. } | BoundExpr::Parameter { .. } | BoundExpr::Variable { .. } => {}
        BoundExpr::Property { base, .. } => out.push(base),
        BoundExpr::Unary { operand, .. } | BoundExpr::IsNull { operand, .. } => out.push(operand),
        BoundExpr::Binary { lhs, rhs, .. } => {
            out.push(lhs);
            out.push(rhs);
        }
        BoundExpr::HasLabels { subject, .. } => out.push(subject),
        BoundExpr::Function { args, .. } => out.extend(args.iter()),
        BoundExpr::Case {
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
        BoundExpr::ListLiteral { items, .. } => out.extend(items.iter()),
        BoundExpr::MapLiteral { entries, .. } => {
            out.extend(entries.iter().map(|(_, value)| value));
        }
        BoundExpr::Index { base, index, .. } => {
            out.push(base);
            out.push(index);
        }
        BoundExpr::Slice { base, from, to, .. } => {
            out.push(base);
            out.extend(from.iter().map(|b| &**b));
            out.extend(to.iter().map(|b| &**b));
        }
        BoundExpr::ListComprehension {
            list,
            where_clause,
            map,
            ..
        } => {
            out.push(list);
            out.extend(where_clause.iter().map(|b| &**b));
            out.extend(map.iter().map(|b| &**b));
        }
        BoundExpr::Quantifier {
            list, predicate, ..
        } => {
            out.push(list);
            out.push(predicate);
        }
        BoundExpr::Reduce {
            init, list, expr, ..
        } => {
            out.push(init);
            out.push(list);
            out.push(expr);
        }
        BoundExpr::PatternComprehension {
            pattern,
            where_clause,
            map,
            ..
        } => {
            out.extend(pattern.start.properties.iter());
            for (rel, node) in &pattern.steps {
                out.extend(rel.properties.iter());
                out.extend(node.properties.iter());
            }
            out.extend(where_clause.iter().map(|b| &**b));
            out.push(map);
        }
        BoundExpr::PatternPredicate { pattern, .. } => {
            out.extend(pattern.start.properties.iter());
            for (rel, node) in &pattern.steps {
                out.extend(rel.properties.iter());
                out.extend(node.properties.iter());
            }
        }
    }
}

/// The grouping-key reference walk over a BOUND expression
/// (acetone-1qj): a node matching one of `keys` (structurally,
/// span-insensitively — for `RefErrorMode::Ambiguous` only simple
/// variable/property nodes may match; ORDER BY may whole-match) is a
/// projected grouping key and stops the walk; aggregates stop it (their
/// arguments are aggregated); a `Variable` is accepted when it is an
/// output alias (`output_ids`) or an iteration-local; anything else that
/// reaches a `Variable` errors per `mode`. `variables` supplies the name
/// for the `UndefinedVariable` rendering.
/// The pre-indexed grouping keys: digest -> keys in that bucket, plus the
/// per-node digests of the expression under validation. Probing is an
/// O(1) average lookup confirmed by `same_bound` (Phase 10 security
/// review MAJOR 2 — the linear scan measured quadratic in query size).
struct KeyIndex<'k> {
    buckets: HashMap<u64, Vec<&'k BoundExpr>>,
    digests: HashMap<usize, u64>,
}

impl<'k> KeyIndex<'k> {
    fn build(keys: &[&'k BoundExpr], probes: &[&BoundExpr]) -> Self {
        let mut buckets: HashMap<u64, Vec<&'k BoundExpr>> = HashMap::new();
        let mut scratch = HashMap::new();
        for key in keys {
            let digest = digest_tree(key, &mut Vec::new(), &mut scratch);
            buckets.entry(digest).or_default().push(*key);
        }
        let mut digests = HashMap::new();
        for probe in probes {
            digest_tree(probe, &mut Vec::new(), &mut digests);
        }
        KeyIndex { buckets, digests }
    }

    fn whole_match(&self, expr: &BoundExpr) -> bool {
        let addr = expr as *const BoundExpr as usize;
        let Some(digest) = self.digests.get(&addr) else {
            return false;
        };
        self.buckets
            .get(digest)
            .is_some_and(|bucket| bucket.iter().any(|key| same_bound(key, expr)))
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_grouping_refs(
    expr: &BoundExpr,
    keys: &KeyIndex,
    output_ids: &[u32],
    mode: RefErrorMode,
    item_span: Span,
    locals: &mut Vec<u32>,
    variables: &[crate::bind::bound::VarBinding],
) -> Result<(), BindError> {
    let simple = matches!(
        expr,
        BoundExpr::Variable { .. } | BoundExpr::Property { .. }
    );
    let whole_match_ok = match mode {
        RefErrorMode::Ambiguous => simple,
        RefErrorMode::Undefined => true,
    };
    if whole_match_ok && keys.whole_match(expr) {
        return Ok(());
    }
    match expr {
        BoundExpr::Aggregate { .. } => Ok(()),
        BoundExpr::Literal { .. } | BoundExpr::Parameter { .. } => Ok(()),
        BoundExpr::Variable { id, span } => {
            if locals.contains(&id.0) || output_ids.contains(&id.0) {
                return Ok(());
            }
            Err(match mode {
                RefErrorMode::Ambiguous => BindError::AmbiguousAggregation { span: item_span },
                RefErrorMode::Undefined => BindError::UndefinedVariable {
                    name: variables
                        .get(id.0 as usize)
                        .map(|v| v.name.clone())
                        .unwrap_or_default(),
                    span: *span,
                },
            })
        }
        BoundExpr::ListComprehension {
            variable,
            list,
            where_clause,
            map,
            ..
        } => {
            validate_grouping_refs(list, keys, output_ids, mode, item_span, locals, variables)?;
            locals.push(variable.0);
            let result = where_clause
                .as_deref()
                .map_or(Ok(()), |e| {
                    validate_grouping_refs(e, keys, output_ids, mode, item_span, locals, variables)
                })
                .and_then(|()| {
                    map.as_deref().map_or(Ok(()), |e| {
                        validate_grouping_refs(
                            e, keys, output_ids, mode, item_span, locals, variables,
                        )
                    })
                });
            locals.pop();
            result
        }
        BoundExpr::Quantifier {
            variable,
            list,
            predicate,
            ..
        } => {
            validate_grouping_refs(list, keys, output_ids, mode, item_span, locals, variables)?;
            locals.push(variable.0);
            let result = validate_grouping_refs(
                predicate, keys, output_ids, mode, item_span, locals, variables,
            );
            locals.pop();
            result
        }
        BoundExpr::Reduce {
            accumulator,
            init,
            variable,
            list,
            expr: body,
            ..
        } => {
            validate_grouping_refs(init, keys, output_ids, mode, item_span, locals, variables)?;
            validate_grouping_refs(list, keys, output_ids, mode, item_span, locals, variables)?;
            locals.push(accumulator.0);
            locals.push(variable.0);
            let result =
                validate_grouping_refs(body, keys, output_ids, mode, item_span, locals, variables);
            locals.pop();
            locals.pop();
            result
        }
        BoundExpr::PatternComprehension {
            pattern,
            where_clause,
            map,
            ..
        } => {
            // Pattern variables shadow outer names for the body — a
            // deliberate over-acceptance in the safe direction (PR #244
            // review finding 7.3).
            let depth = locals.len();
            locals.extend(pattern.start.var.iter().map(|v| v.0));
            for (rel, node) in &pattern.steps {
                locals.extend(rel.var.iter().map(|v| v.0));
                locals.extend(node.var.iter().map(|v| v.0));
            }
            let mut result = Ok(());
            for e in
                pattern
                    .start
                    .properties
                    .iter()
                    .chain(pattern.steps.iter().flat_map(|(rel, node)| {
                        rel.properties.iter().chain(node.properties.iter())
                    }))
                    .chain(where_clause.as_deref())
                    .chain(std::iter::once(&**map))
            {
                result =
                    validate_grouping_refs(e, keys, output_ids, mode, item_span, locals, variables);
                if result.is_err() {
                    break;
                }
            }
            locals.truncate(depth);
            result
        }
        other => {
            let mut children: Vec<&BoundExpr> = Vec::new();
            push_bound_children(other, &mut children);
            for child in children {
                validate_grouping_refs(
                    child, keys, output_ids, mode, item_span, locals, variables,
                )?;
            }
            Ok(())
        }
    }
}

/// Which error a disallowed reference raises (acetone-1qj): a projection
/// item's mixture is `AmbiguousAggregationExpression`; an ORDER BY
/// reference that fails to reduce is `UndefinedVariable` (the name has no
/// value in the post-projection row).
#[derive(Clone, Copy)]
enum RefErrorMode {
    Ambiguous,
    Undefined,
}

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
            BoundExpr::HasLabels { subject, .. } => stack.push(subject),
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
            BoundExpr::PatternComprehension {
                pattern,
                where_clause,
                map,
                ..
            } => {
                stack.extend(pattern.start.properties.iter());
                for (rel, node) in &pattern.steps {
                    stack.extend(rel.properties.iter());
                    stack.extend(node.properties.iter());
                }
                stack.extend(where_clause.iter().map(|b| &**b));
                stack.push(map);
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
