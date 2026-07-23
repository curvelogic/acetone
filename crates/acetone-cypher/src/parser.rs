//! Recursive-descent parser with Pratt-style expression precedence for the
//! v0.1 read subset (spec §5.1 Level R), the `AT <ref>` extension and
//! `CALL ... YIELD` (spec §5.2).
//!
//! Hard properties, enforced by fuzz tests:
//! - never panics on any input;
//! - expression nesting is bounded by an explicit depth limit (queries are
//!   untrusted input; unbounded recursion is a stack-overflow abort);
//! - every error carries a byte span into the source.

use crate::ast::*;
use crate::error::ParseError;
use crate::lex::{Token, TokenKind, lex};
use crate::span::Span;

/// Maximum parse-time recursion depth (parenthesis/list/map/argument
/// nesting). Deep enough for any query a human or the TCK writes; shallow
/// enough that the ~13 recursive-descent frames per nesting level stay
/// comfortably inside a 2 MiB stack (the Rust test-thread default) even in
/// debug builds, where frames are at their fattest.
pub const MAX_DEPTH: usize = 64;

/// Maximum total AST depth [`parse`] will return, enforced by an
/// iterative post-parse measurement ([`Query::depth`]). Loop-folded
/// operator chains (`a + b + ...`, `NOT NOT ...`, `x.a.b...`) nest the
/// AST without recursing at parse time, so a parse-time recursion guard
/// alone cannot bound this. The bound is what makes recursive walks over
/// the AST (binder, planner) safe; teardown of deeper transients inside
/// `parse` itself is safe regardless because `Expr::drop` is iterative.
/// Loose enough for machine-generated queries with a few hundred chained
/// terms.
pub const MAX_AST_DEPTH: usize = 256;

/// Words that cannot be used as variable names, aliases or yield items
/// unless backquoted. Labels, relationship types, property names, map keys
/// and procedure names are deliberately not restricted — openCypher keeps
/// keywords legal in those positions.
const RESERVED: &[&str] = &[
    "ALL",
    "AND",
    "AS",
    "ASC",
    "ASCENDING",
    "BY",
    "CALL",
    "CASE",
    "CONTAINS",
    "CREATE",
    "DELETE",
    "DESC",
    "DESCENDING",
    "DETACH",
    "DISTINCT",
    "ELSE",
    "END",
    "ENDS",
    "EXISTS",
    "FALSE",
    "IN",
    "IS",
    "LIMIT",
    "MATCH",
    "MERGE",
    "NOT",
    "NULL",
    "ON",
    "OPTIONAL",
    "OR",
    "ORDER",
    "REMOVE",
    "RETURN",
    "SET",
    "SKIP",
    "STARTS",
    "THEN",
    "TRUE",
    "UNION",
    "UNWIND",
    "WHEN",
    "WHERE",
    "WITH",
    "XOR",
    "YIELD",
];

fn is_reserved(word: &str) -> bool {
    RESERVED.iter().any(|r| word.eq_ignore_ascii_case(r))
}

/// Parse a single openCypher query into a spanned AST.
///
/// Guarantees, whatever the input: no panics, bounded parse-time
/// recursion ([`MAX_DEPTH`]), and any returned `Query` has expression
/// nesting no deeper than [`MAX_AST_DEPTH`].
pub fn parse(input: &str) -> Result<Query, ParseError> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        source: input,
        tokens,
        pos: 0,
        depth: 0,
    };
    let query = parser.query()?;
    parser.expect_end()?;
    if query.depth() > MAX_AST_DEPTH {
        return Err(ParseError::RecursionLimit {
            limit: MAX_AST_DEPTH,
            span: query.span,
        });
    }
    Ok(query)
}

/// The quantifier keyword, if `name` names one.
fn quantifier_kind(name: &str) -> Option<QuantifierKind> {
    match () {
        _ if name.eq_ignore_ascii_case("all") => Some(QuantifierKind::All),
        _ if name.eq_ignore_ascii_case("any") => Some(QuantifierKind::Any),
        _ if name.eq_ignore_ascii_case("none") => Some(QuantifierKind::None),
        _ if name.eq_ignore_ascii_case("single") => Some(QuantifierKind::Single),
        _ => None,
    }
}

struct Parser<'a> {
    /// The query text the tokens were lexed from, for diagnostics that
    /// quote raw source (e.g. the bare-refspec suggestion after `AT`).
    source: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    // --- token plumbing ----------------------------------------------------

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, n: usize) -> &Token {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)]
    }

    fn bump(&mut self) -> Token {
        let token = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        token
    }

    fn at(&self, kind: &TokenKind) -> bool {
        &self.peek().kind == kind
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, expected: &str) -> Result<Token, ParseError> {
        if self.at(kind) {
            Ok(self.bump())
        } else {
            Err(self.unexpected(expected))
        }
    }

    fn unexpected(&self, expected: &str) -> ParseError {
        ParseError::Unexpected {
            expected: expected.into(),
            found: self.peek().kind.describe(),
            span: self.peek().span,
        }
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    /// Non-backquoted keyword check, case-insensitive, without consuming.
    fn at_kw(&self, kw: &str) -> bool {
        matches!(
            &self.peek().kind,
            TokenKind::Ident { name, backquoted: false } if name.eq_ignore_ascii_case(kw)
        )
    }

    fn at_kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(
            &self.peek_at(n).kind,
            TokenKind::Ident { name, backquoted: false } if name.eq_ignore_ascii_case(kw)
        )
    }

    fn at_kw2(&self, kw1: &str, kw2: &str) -> bool {
        self.at_kw(kw1) && self.at_kw_at(1, kw2)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<Token, ParseError> {
        if self.at_kw(kw) {
            Ok(self.bump())
        } else {
            Err(self.unexpected(kw))
        }
    }

    /// Any identifier, keyword or not. For positions where openCypher
    /// leaves keywords legal: labels, relationship types, property names,
    /// map keys, procedure names.
    fn any_name(&mut self, expected: &str) -> Result<(String, Span), ParseError> {
        match self.peek().kind.clone() {
            TokenKind::Ident { name, .. } => {
                let span = self.bump().span;
                Ok((name, span))
            }
            _ => Err(self.unexpected(expected)),
        }
    }

    /// An identifier in a binding position: reserved words rejected unless
    /// backquoted.
    fn binding_name(
        &mut self,
        expected: &str,
        usage: &'static str,
    ) -> Result<(String, Span), ParseError> {
        match self.peek().kind.clone() {
            TokenKind::Ident { name, backquoted } => {
                if !backquoted && is_reserved(&name) {
                    return Err(ParseError::ReservedWord {
                        word: name,
                        usage,
                        span: self.peek().span,
                    });
                }
                let span = self.bump().span;
                Ok((name, span))
            }
            _ => Err(self.unexpected(expected)),
        }
    }

    fn expect_end(&mut self) -> Result<(), ParseError> {
        self.eat(&TokenKind::Semicolon);
        if self.at(&TokenKind::Eof) {
            Ok(())
        } else {
            Err(self.unexpected("end of query"))
        }
    }

    // --- query structure -----------------------------------------------------

    fn query(&mut self) -> Result<Query, ParseError> {
        let start = self.peek().span;
        let mut clauses = Vec::new();
        while !self.at(&TokenKind::Eof) && !self.at(&TokenKind::Semicolon) {
            if let Some(previous) = clauses.last()
                && matches!(previous, Clause::Return(_))
            {
                return Err(ParseError::QueryStructure {
                    message: "no clause may follow RETURN".into(),
                    span: self.peek().span,
                });
            }
            clauses.push(self.clause()?);
        }
        let span = start.to(self.prev_span());
        match clauses.last() {
            None => Err(ParseError::QueryStructure {
                message: "empty query".into(),
                span: Span::new(0, 0),
            }),
            // A query ends on RETURN, a standalone CALL, or a write clause
            // (a bare `CREATE ...` with no trailing projection).
            Some(Clause::Return(_)) | Some(Clause::Call(_)) => Ok(Query { clauses, span }),
            Some(clause) if clause.is_write() => Ok(Query { clauses, span }),
            Some(other) => Err(ParseError::QueryStructure {
                message: "query must end with RETURN, a write clause, or a standalone CALL".into(),
                span: other.span(),
            }),
        }
    }

    fn clause(&mut self) -> Result<Clause, ParseError> {
        if self.at_kw2("OPTIONAL", "MATCH") {
            let start = self.bump().span;
            self.bump();
            return self.match_clause(true, start);
        }
        if self.at_kw("MATCH") {
            let start = self.bump().span;
            return self.match_clause(false, start);
        }
        if self.at_kw("UNWIND") {
            let start = self.bump().span;
            let expr = self.expression()?;
            self.expect_kw("AS")?;
            let (alias, end) = self.binding_name("alias after AS", "an alias")?;
            return Ok(Clause::Unwind(UnwindClause {
                expr,
                alias,
                span: start.to(end),
            }));
        }
        if self.at_kw("WITH") {
            let start = self.bump().span;
            return Ok(Clause::With(self.projection(start, true)?));
        }
        if self.at_kw("RETURN") {
            let start = self.bump().span;
            return Ok(Clause::Return(self.projection(start, false)?));
        }
        if self.at_kw("CALL") {
            return self.call_clause();
        }
        if self.at_kw("CREATE") {
            let start = self.bump().span;
            let patterns = self.pattern_list()?;
            return Ok(Clause::Create(CreateClause {
                patterns,
                span: start.to(self.prev_span()),
            }));
        }
        if self.at_kw("SET") {
            let start = self.bump().span;
            let mut items = vec![self.set_item()?];
            while self.eat(&TokenKind::Comma) {
                items.push(self.set_item()?);
            }
            return Ok(Clause::Set(SetClause {
                items,
                span: start.to(self.prev_span()),
            }));
        }
        if self.at_kw("REMOVE") {
            let start = self.bump().span;
            let mut items = vec![self.remove_item()?];
            while self.eat(&TokenKind::Comma) {
                items.push(self.remove_item()?);
            }
            return Ok(Clause::Remove(RemoveClause {
                items,
                span: start.to(self.prev_span()),
            }));
        }
        if self.at_kw2("DETACH", "DELETE") {
            let start = self.bump().span;
            self.bump();
            return self.delete_clause(true, start);
        }
        if self.at_kw("DELETE") {
            let start = self.bump().span;
            return self.delete_clause(false, start);
        }
        if self.at_kw("MERGE") {
            return self.merge_clause();
        }
        Err(self.unexpected(
            "a clause (MATCH, OPTIONAL MATCH, UNWIND, WITH, RETURN, CALL, CREATE, SET, REMOVE, DELETE or DETACH DELETE)",
        ))
    }

    /// `MERGE <pattern> (ON CREATE SET ... | ON MATCH SET ...)*`.
    fn merge_clause(&mut self) -> Result<Clause, ParseError> {
        let start = self.expect_kw("MERGE")?.span;
        let pattern = self.path_pattern()?;
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        loop {
            if self.at_kw2("ON", "CREATE") {
                self.bump();
                self.bump();
                self.expect_kw("SET")?;
                on_create.push(self.set_item()?);
                while self.eat(&TokenKind::Comma) {
                    on_create.push(self.set_item()?);
                }
            } else if self.at_kw2("ON", "MATCH") {
                self.bump();
                self.bump();
                self.expect_kw("SET")?;
                on_match.push(self.set_item()?);
                while self.eat(&TokenKind::Comma) {
                    on_match.push(self.set_item()?);
                }
            } else {
                break;
            }
        }
        Ok(Clause::Merge(MergeClause {
            pattern,
            on_create,
            on_match,
            span: start.to(self.prev_span()),
        }))
    }

    /// `DELETE e1, e2, ...` — the targets are ordinary expressions that must
    /// evaluate to nodes, relationships or paths.
    fn delete_clause(&mut self, detach: bool, start: Span) -> Result<Clause, ParseError> {
        let mut targets = vec![self.expression()?];
        while self.eat(&TokenKind::Comma) {
            targets.push(self.expression()?);
        }
        Ok(Clause::Delete(DeleteClause {
            detach,
            targets,
            span: start.to(self.prev_span()),
        }))
    }

    /// One `SET` assignment: `x.key = v`, `x = map`, `x += map`, or `x:A:B`.
    fn set_item(&mut self) -> Result<SetItem, ParseError> {
        let (var, var_span) = self.binding_name("a variable", "a SET target")?;
        if self.at(&TokenKind::Colon) {
            let mut labels = Vec::new();
            while self.eat(&TokenKind::Colon) {
                labels.push(self.any_name("a label")?.0);
            }
            return Ok(SetItem::AddLabels {
                var,
                labels,
                span: var_span.to(self.prev_span()),
            });
        }
        if self.eat(&TokenKind::Dot) {
            let (key, _) = self.any_name("a property name after '.'")?;
            self.expect(&TokenKind::Eq, "'=' in a SET assignment")?;
            let value = self.expression()?;
            return Ok(SetItem::Property {
                var,
                key,
                value,
                span: var_span.to(self.prev_span()),
            });
        }
        // `+=` lexes as Plus then Eq (there is no combined token).
        if self.at(&TokenKind::Plus) && self.peek_at(1).kind == TokenKind::Eq {
            self.bump();
            self.bump();
            let value = self.expression()?;
            return Ok(SetItem::Merge {
                var,
                value,
                span: var_span.to(self.prev_span()),
            });
        }
        self.expect(
            &TokenKind::Eq,
            "'.property', ':Label', '=' or '+=' after a SET target",
        )?;
        let value = self.expression()?;
        Ok(SetItem::Replace {
            var,
            value,
            span: var_span.to(self.prev_span()),
        })
    }

    /// One `REMOVE` item: `x.key` or `x:A:B`.
    fn remove_item(&mut self) -> Result<RemoveItem, ParseError> {
        let (var, var_span) = self.binding_name("a variable", "a REMOVE target")?;
        if self.at(&TokenKind::Colon) {
            let mut labels = Vec::new();
            while self.eat(&TokenKind::Colon) {
                labels.push(self.any_name("a label")?.0);
            }
            return Ok(RemoveItem::Labels {
                var,
                labels,
                span: var_span.to(self.prev_span()),
            });
        }
        self.expect(
            &TokenKind::Dot,
            "'.property' or ':Label' after a REMOVE target",
        )?;
        let (key, _) = self.any_name("a property name after '.'")?;
        Ok(RemoveItem::Property {
            var,
            key,
            span: var_span.to(self.prev_span()),
        })
    }

    fn match_clause(&mut self, optional: bool, start: Span) -> Result<Clause, ParseError> {
        let patterns = self.pattern_list()?;
        // Acetone extension (spec §5.2): `AT <refspec>` suffixes the MATCH
        // clause group, before any WHERE. One token of lookahead; `AT` is
        // not a reserved word — in this position no bare identifier can
        // follow a pattern list, so the grammar stays unambiguous.
        let at_ref = if self.at_kw("AT") {
            self.bump();
            match self.peek().kind.clone() {
                TokenKind::Str(value) => {
                    let span = self.bump().span;
                    Some(AtRef::Refspec { value, span })
                }
                TokenKind::Parameter(name) => {
                    let span = self.bump().span;
                    Some(AtRef::Parameter { name, span })
                }
                _ => return Err(self.bare_refspec_error()),
            }
        } else {
            None
        };
        let where_clause = if self.eat_kw("WHERE") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(Clause::Match(MatchClause {
            optional,
            patterns,
            at_ref,
            where_clause,
            span: start.to(self.prev_span()),
        }))
    }

    /// The error for a non-string, non-parameter token after `AT`
    /// (acetone-dm3). A bare commit hash or branch name — which may lex as
    /// several adjacent tokens: `3db804f9` is integer `3` then ident
    /// `db804f9` — gets an actionable suggestion quoting the raw source
    /// text. Anything else (a reserved word, punctuation, end of input)
    /// keeps the generic message: the refspec is missing, not unquoted.
    fn bare_refspec_error(&self) -> ParseError {
        let refspec_like = match &self.peek().kind {
            TokenKind::Integer(_) | TokenKind::IntegerTooLarge(_) | TokenKind::Float(_) => true,
            TokenKind::Ident {
                name,
                backquoted: false,
            } => !is_reserved(name),
            _ => false,
        };
        if !refspec_like {
            return self.unexpected("a refspec string or parameter after AT");
        }
        // Reassemble the refspec the user wrote: the run of tokens with no
        // intervening whitespace, sliced from the source (token-level
        // reconstruction would be lossy — `4e5c0148` lexes through a
        // float).
        let start = self.peek().span;
        let mut end = start;
        let mut n = 1;
        loop {
            let next = self.peek_at(n);
            if next.kind == TokenKind::Eof || next.span.start != end.end {
                break;
            }
            end = next.span;
            n += 1;
        }
        let raw = &self.source[start.start..end.end];
        ParseError::QueryStructure {
            message: format!(
                "a refspec after AT must be a string literal or a parameter \
                 — try AT '{raw}' (or AT $ref)"
            ),
            span: Span::new(start.start, end.end),
        }
    }

    fn projection(&mut self, start: Span, is_with: bool) -> Result<Projection, ParseError> {
        let distinct = self.eat_kw("DISTINCT");
        let mut items = Vec::new();
        loop {
            if self.at(&TokenKind::Star) {
                let span = self.bump().span;
                items.push(ProjectionItem::Star { span });
            } else {
                let expr = self.expression()?;
                let alias = if self.eat_kw("AS") {
                    Some(self.binding_name("alias after AS", "an alias")?.0)
                } else {
                    None
                };
                let span = expr.span().to(self.prev_span());
                items.push(ProjectionItem::Expr { expr, alias, span });
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let mut order_by = Vec::new();
        if self.at_kw2("ORDER", "BY") {
            self.bump();
            self.bump();
            loop {
                let expr = self.expression()?;
                let descending = if self.eat_kw("DESC") || self.eat_kw("DESCENDING") {
                    true
                } else {
                    let _ = self.eat_kw("ASC") || self.eat_kw("ASCENDING");
                    false
                };
                order_by.push(SortItem { expr, descending });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat_kw("SKIP") {
            Some(self.expression()?)
        } else {
            None
        };
        let limit = if self.eat_kw("LIMIT") {
            Some(self.expression()?)
        } else {
            None
        };
        let where_clause = if is_with && self.eat_kw("WHERE") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(Projection {
            distinct,
            items,
            order_by,
            skip,
            limit,
            where_clause,
            span: start.to(self.prev_span()),
        })
    }

    fn call_clause(&mut self) -> Result<Clause, ParseError> {
        let start = self.expect_kw("CALL")?.span;
        let mut procedure = vec![self.any_name("a procedure name")?.0];
        while self.eat(&TokenKind::Dot) {
            procedure.push(self.any_name("a procedure name segment")?.0);
        }
        let mut args = Vec::new();
        if self.eat(&TokenKind::LParen) {
            if !self.at(&TokenKind::RParen) {
                loop {
                    args.push(self.expression()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RParen, "')' after procedure arguments")?;
        }
        let mut yield_items = Vec::new();
        if self.eat_kw("YIELD") {
            loop {
                yield_items.push(self.binding_name("a yield column", "a yield item")?.0);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let where_clause = if self.eat_kw("WHERE") {
            Some(self.expression()?)
        } else {
            None
        };
        Ok(Clause::Call(CallClause {
            procedure,
            args,
            yield_items,
            where_clause,
            span: start.to(self.prev_span()),
        }))
    }

    // --- patterns ----------------------------------------------------------

    fn pattern_list(&mut self) -> Result<Vec<PathPattern>, ParseError> {
        let mut patterns = vec![self.path_pattern()?];
        while self.eat(&TokenKind::Comma) {
            patterns.push(self.path_pattern()?);
        }
        Ok(patterns)
    }

    fn path_pattern(&mut self) -> Result<PathPattern, ParseError> {
        let variable = if matches!(self.peek().kind, TokenKind::Ident { .. })
            && self.peek_at(1).kind == TokenKind::Eq
        {
            let (name, _) = self.binding_name("a path variable", "a path variable")?;
            self.bump(); // =
            Some(name)
        } else {
            None
        };
        let start_span = self.peek().span;
        let start = self.node_pattern()?;
        let mut steps = Vec::new();
        while self.at(&TokenKind::Minus)
            || self.at(&TokenKind::LArrow)
            || self.at(&TokenKind::Arrow)
        {
            let rel = self.rel_pattern()?;
            let node = self.node_pattern()?;
            steps.push((rel, node));
        }
        Ok(PathPattern {
            variable,
            start,
            steps,
            span: start_span.to(self.prev_span()),
        })
    }

    fn node_pattern(&mut self) -> Result<NodePattern, ParseError> {
        let open = self.expect(&TokenKind::LParen, "'(' to open a node pattern")?;
        let variable = if matches!(self.peek().kind, TokenKind::Ident { .. }) {
            Some(self.binding_name("a variable", "a variable")?.0)
        } else {
            None
        };
        let mut labels = Vec::new();
        while self.eat(&TokenKind::Colon) {
            labels.push(self.any_name("a label")?.0);
        }
        let properties = self.pattern_properties()?;
        let close = self.expect(&TokenKind::RParen, "')' to close the node pattern")?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
            span: open.span.to(close.span),
        })
    }

    fn rel_pattern(&mut self) -> Result<RelPattern, ParseError> {
        let start = self.peek().span;
        let incoming = self.eat(&TokenKind::LArrow);
        if !incoming {
            self.expect(&TokenKind::Minus, "'-' to open a relationship pattern")?;
        }

        let mut variable = None;
        let mut types = Vec::new();
        let mut var_length = None;
        let mut properties = None;

        if self.eat(&TokenKind::LBracket) {
            if matches!(self.peek().kind, TokenKind::Ident { .. }) {
                variable = Some(self.binding_name("a variable", "a variable")?.0);
            }
            if self.eat(&TokenKind::Colon) {
                types.push(self.any_name("a relationship type")?.0);
                while self.eat(&TokenKind::Pipe) {
                    // Tolerate the legacy `|:TYPE` alternative form.
                    self.eat(&TokenKind::Colon);
                    types.push(self.any_name("a relationship type")?.0);
                }
            }
            if self.eat(&TokenKind::Star) {
                var_length = Some(self.var_length()?);
            }
            properties = self.pattern_properties()?;
            self.expect(&TokenKind::RBracket, "']' to close the relationship detail")?;
        }

        let outgoing = if self.eat(&TokenKind::Arrow) {
            true
        } else {
            self.expect(
                &TokenKind::Minus,
                "'-' or '->' to close the relationship pattern",
            )?;
            false
        };

        let direction = match (incoming, outgoing) {
            (true, false) => Direction::In,
            (false, true) => Direction::Out,
            (false, false) => Direction::Undirected,
            (true, true) => {
                return Err(ParseError::Unexpected {
                    expected: "a relationship pointing one way".into(),
                    found: "'<-...->'".into(),
                    span: start.to(self.prev_span()),
                });
            }
        };
        Ok(RelPattern {
            variable,
            types,
            direction,
            var_length,
            properties,
            span: start.to(self.prev_span()),
        })
    }

    /// Property map on a node or relationship pattern: a map literal or a
    /// parameter.
    fn pattern_properties(&mut self) -> Result<Option<Expr>, ParseError> {
        if self.at(&TokenKind::LBrace) {
            return Ok(Some(self.map_literal()?));
        }
        if let TokenKind::Parameter(name) = self.peek().kind.clone() {
            let span = self.bump().span;
            return Ok(Some(Expr::Parameter { name, span }));
        }
        Ok(None)
    }

    /// After a consumed `*`: `*`, `*n`, `*n..m`, `*n..`, `*..m`.
    fn var_length(&mut self) -> Result<VarLength, ParseError> {
        let mut bounds = VarLength::default();
        if let TokenKind::Integer(n) = self.peek().kind {
            let span = self.peek().span;
            let n = u64::try_from(n).map_err(|_| ParseError::Unexpected {
                expected: "a non-negative length bound".into(),
                found: format!("{n}"),
                span,
            })?;
            self.bump();
            bounds.min = Some(n);
            if self.eat(&TokenKind::DotDot) {
                if let TokenKind::Integer(m) = self.peek().kind {
                    let span = self.peek().span;
                    let m = u64::try_from(m).map_err(|_| ParseError::Unexpected {
                        expected: "a non-negative length bound".into(),
                        found: format!("{m}"),
                        span,
                    })?;
                    self.bump();
                    bounds.max = Some(m);
                }
            } else {
                bounds.max = bounds.min;
            }
        } else if self.eat(&TokenKind::DotDot)
            && let TokenKind::Integer(m) = self.peek().kind
        {
            let span = self.peek().span;
            let m = u64::try_from(m).map_err(|_| ParseError::Unexpected {
                expected: "a non-negative length bound".into(),
                found: format!("{m}"),
                span,
            })?;
            self.bump();
            bounds.max = Some(m);
        }
        Ok(bounds)
    }

    // --- expressions ---------------------------------------------------------

    /// The single depth-guarded entry point for expression parsing. All
    /// nested expression positions (parentheses, arguments, list/map
    /// elements, comprehension bodies, index/slice operands) come back
    /// through here, so nesting depth is bounded by [`MAX_DEPTH`].
    ///
    /// Operator chains folded by loops (`a OR b OR ...`, `NOT NOT ...`,
    /// `x.a.b.c...`) nest the AST without recursing here; the total AST
    /// depth they can build is bounded separately by the post-parse
    /// [`ast_depth`] check in [`parse`], and teardown of any transient
    /// deep AST is safe because [`Expr`]'s `Drop` is iterative.
    fn expression(&mut self) -> Result<Expr, ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(ParseError::RecursionLimit {
                limit: MAX_DEPTH,
                span: self.peek().span,
            });
        }
        self.depth += 1;
        let result = self.expr_or();
        self.depth -= 1;
        result
    }

    fn expr_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_xor()?;
        while self.eat_kw("OR") {
            let rhs = self.expr_xor()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op: BinaryOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn expr_xor(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_and()?;
        while self.eat_kw("XOR") {
            let rhs = self.expr_and()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op: BinaryOp::Xor,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn expr_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_not()?;
        while self.eat_kw("AND") {
            let rhs = self.expr_not()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op: BinaryOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    /// `NOT` chains are folded iteratively so adversarial `NOT NOT NOT ...`
    /// input cannot recurse.
    fn expr_not(&mut self) -> Result<Expr, ParseError> {
        let mut nots: Vec<Span> = Vec::new();
        while self.at_kw("NOT") {
            nots.push(self.bump().span);
        }
        let mut expr = self.expr_comparison()?;
        for span in nots.into_iter().rev() {
            let full = span.to(expr.span());
            expr = Expr::Unary {
                op: UnaryOp::Not,
                operand: Box::new(expr),
                span: full,
            };
        }
        Ok(expr)
    }

    fn expr_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_string_list()?;
        loop {
            let op = if self.eat(&TokenKind::Eq) {
                BinaryOp::Eq
            } else if self.eat(&TokenKind::Ne) {
                BinaryOp::Ne
            } else if self.eat(&TokenKind::Le) {
                BinaryOp::Le
            } else if self.eat(&TokenKind::Ge) {
                BinaryOp::Ge
            } else if self.eat(&TokenKind::Lt) {
                BinaryOp::Lt
            } else if self.eat(&TokenKind::Gt) {
                BinaryOp::Gt
            } else {
                break;
            };
            let rhs = self.expr_string_list()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    /// String/list operators (`IN`, `STARTS WITH`, `ENDS WITH`,
    /// `CONTAINS`, `=~`) bind tighter than comparison, per openCypher
    /// (TCK Precedence1 [11], Precedence3 [6]: `false = true IN [...]`
    /// is `false = (true IN [...])`).
    fn expr_string_list(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_additive()?;
        loop {
            let op = if self.eat(&TokenKind::RegexEq) {
                BinaryOp::RegexMatch
            } else if self.at_kw("IN") {
                self.bump();
                BinaryOp::In
            } else if self.at_kw2("STARTS", "WITH") {
                self.bump();
                self.bump();
                BinaryOp::StartsWith
            } else if self.at_kw2("ENDS", "WITH") {
                self.bump();
                self.bump();
                BinaryOp::EndsWith
            } else if self.at_kw("CONTAINS") {
                self.bump();
                BinaryOp::Contains
            } else {
                break;
            };
            let rhs = self.expr_additive()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn expr_additive(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_multiplicative()?;
        loop {
            let op = if self.eat(&TokenKind::Plus) {
                BinaryOp::Add
            } else if self.eat(&TokenKind::Minus) {
                BinaryOp::Sub
            } else {
                break;
            };
            let rhs = self.expr_multiplicative()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn expr_multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_power()?;
        loop {
            let op = if self.eat(&TokenKind::Star) {
                BinaryOp::Mul
            } else if self.eat(&TokenKind::Slash) {
                BinaryOp::Div
            } else if self.eat(&TokenKind::Percent) {
                BinaryOp::Mod
            } else {
                break;
            };
            let rhs = self.expr_power()?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    /// Left-associative `^` (openCypher/Neo4j semantics, pinned by TCK
    /// Precedence2: `4 ^ (3 * 2) ^ 3` is `(4^6)^3`), folded in a loop.
    fn expr_power(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.expr_unary()?;
        while self.eat(&TokenKind::Caret) {
            let rhs = self.expr_unary()?;
            let span = expr.span().to(rhs.span());
            expr = Expr::Binary {
                op: BinaryOp::Pow,
                lhs: Box::new(expr),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(expr)
    }

    /// Prefix `+`/`-` chains are folded iteratively so `----5` cannot
    /// recurse.
    fn expr_unary(&mut self) -> Result<Expr, ParseError> {
        let mut prefixes: Vec<(UnaryOp, Span)> = Vec::new();
        loop {
            if self.at(&TokenKind::Minus) {
                prefixes.push((UnaryOp::Minus, self.bump().span));
            } else if self.at(&TokenKind::Plus) {
                prefixes.push((UnaryOp::Plus, self.bump().span));
            } else {
                break;
            }
        }
        // openCypher's smallest integer: a magnitude of exactly 2^63 only
        // fits i64 when negated, so the innermost unary minus folds into
        // the literal (TCK Literals2–4 "Return the smallest integer").
        // Folding is token-level, so `- 9223372036854775808` folds too;
        // any other route to an `IntegerTooLarge` token — bare, a larger
        // magnitude, or a *binary* minus — errors in `expr_atom`.
        let mut expr = match self.peek().kind {
            TokenKind::IntegerTooLarge(magnitude)
                if magnitude == i64::MIN.unsigned_abs()
                    && matches!(prefixes.last(), Some((UnaryOp::Minus, _))) =>
            {
                let token_span = self.bump().span;
                let (_, minus_span) = prefixes.pop().expect("last prefix checked above");
                let literal = Expr::Literal {
                    value: Literal::Integer(i64::MIN),
                    span: minus_span.to(token_span),
                };
                self.expr_postfix_from(literal)?
            }
            _ => self.expr_postfix()?,
        };
        for (op, span) in prefixes.into_iter().rev() {
            let full = span.to(expr.span());
            expr = Expr::Unary {
                op,
                operand: Box::new(expr),
                span: full,
            };
        }
        Ok(expr)
    }

    fn expr_postfix(&mut self) -> Result<Expr, ParseError> {
        let atom = self.expr_atom()?;
        self.expr_postfix_from(atom)
    }

    /// The postfix loop (property access, indexing/slicing, `IS [NOT]
    /// NULL`) applied to an already-parsed head expression.
    fn expr_postfix_from(&mut self, expr: Expr) -> Result<Expr, ParseError> {
        let mut expr = expr;
        loop {
            if self.at(&TokenKind::Dot) {
                self.bump();
                let (key, end) = self.any_name("a property name after '.'")?;
                let span = expr.span().to(end);
                expr = Expr::Property {
                    base: Box::new(expr),
                    key,
                    span,
                };
            } else if self.at(&TokenKind::LBracket) {
                self.bump();
                expr = self.index_or_slice(expr)?;
            } else if self.at_kw2("IS", "NULL") {
                let end = self.peek_at(1).span;
                self.bump();
                self.bump();
                let span = expr.span().to(end);
                expr = Expr::IsNull {
                    operand: Box::new(expr),
                    negated: false,
                    span,
                };
            } else if self.at_kw2("IS", "NOT") && self.at_kw_at(2, "NULL") {
                let end = self.peek_at(2).span;
                self.bump();
                self.bump();
                self.bump();
                let span = expr.span().to(end);
                expr = Expr::IsNull {
                    operand: Box::new(expr),
                    negated: true,
                    span,
                };
            } else {
                break;
            }
        }
        Ok(expr)
    }

    /// After a consumed `[`: `[e]`, `[a..b]`, `[..b]`, `[a..]`, `[..]`.
    fn index_or_slice(&mut self, base: Expr) -> Result<Expr, ParseError> {
        if self.eat(&TokenKind::DotDot) {
            let to = if self.at(&TokenKind::RBracket) {
                None
            } else {
                Some(Box::new(self.expression()?))
            };
            let close = self.expect(&TokenKind::RBracket, "']' to close the slice")?;
            let span = base.span().to(close.span);
            return Ok(Expr::Slice {
                base: Box::new(base),
                from: None,
                to,
                span,
            });
        }
        let first = self.expression()?;
        if self.eat(&TokenKind::DotDot) {
            let to = if self.at(&TokenKind::RBracket) {
                None
            } else {
                Some(Box::new(self.expression()?))
            };
            let close = self.expect(&TokenKind::RBracket, "']' to close the slice")?;
            let span = base.span().to(close.span);
            Ok(Expr::Slice {
                base: Box::new(base),
                from: Some(Box::new(first)),
                to,
                span,
            })
        } else {
            let close = self.expect(&TokenKind::RBracket, "']' to close the index")?;
            let span = base.span().to(close.span);
            Ok(Expr::Index {
                base: Box::new(base),
                index: Box::new(first),
                span,
            })
        }
    }

    fn expr_atom(&mut self) -> Result<Expr, ParseError> {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Integer(n) => {
                self.bump();
                Ok(Expr::Literal {
                    value: Literal::Integer(n),
                    span: token.span,
                })
            }
            // Not directly preceded by a foldable unary minus (see
            // `expr_unary`), so this magnitude cannot fit an i64.
            TokenKind::IntegerTooLarge(magnitude) => Err(ParseError::Lex {
                message: format!(
                    "integer literal {magnitude} is out of range \
                     (integers span -9223372036854775808 to 9223372036854775807)"
                ),
                span: token.span,
            }),
            TokenKind::Float(x) => {
                self.bump();
                Ok(Expr::Literal {
                    value: Literal::Float(x),
                    span: token.span,
                })
            }
            TokenKind::Str(s) => {
                self.bump();
                Ok(Expr::Literal {
                    value: Literal::String(s),
                    span: token.span,
                })
            }
            TokenKind::Parameter(name) => {
                self.bump();
                Ok(Expr::Parameter {
                    name,
                    span: token.span,
                })
            }
            TokenKind::LBracket => self.list_literal_or_comprehension(),
            TokenKind::LBrace => self.map_literal(),
            TokenKind::LParen => self.paren_expr_or_pattern_predicate(),
            TokenKind::Ident {
                ref name,
                backquoted,
            } => {
                if !backquoted {
                    if name.eq_ignore_ascii_case("null") {
                        self.bump();
                        return Ok(Expr::Literal {
                            value: Literal::Null,
                            span: token.span,
                        });
                    }
                    if name.eq_ignore_ascii_case("true") {
                        self.bump();
                        return Ok(Expr::Literal {
                            value: Literal::Boolean(true),
                            span: token.span,
                        });
                    }
                    if name.eq_ignore_ascii_case("false") {
                        self.bump();
                        return Ok(Expr::Literal {
                            value: Literal::Boolean(false),
                            span: token.span,
                        });
                    }
                    if name.eq_ignore_ascii_case("case") {
                        return self.case_expression();
                    }
                    // List-predicate quantifiers and reduce have their own
                    // argument grammar (`x IN list WHERE p`, `acc = init,
                    // x IN list | e`), distinguished from a same-named
                    // function only when the parenthesised body opens with
                    // that shape.
                    if let Some(kind) = quantifier_kind(name)
                        && self.at_quantifier_body()
                    {
                        return self.quantifier(kind);
                    }
                    if name.eq_ignore_ascii_case("reduce") && self.at_reduce_body() {
                        return self.reduce_expression();
                    }
                }
                self.variable_or_function_call()
            }
            _ => Err(self.unexpected("an expression")),
        }
    }

    /// `name(...)`, `ns.name(...)`, `count(*)`, `collect(DISTINCT x)` or a
    /// plain variable. Dotted segments become part of the function name
    /// only when the chain ends in `(`; otherwise the postfix loop treats
    /// them as property accesses.
    fn variable_or_function_call(&mut self) -> Result<Expr, ParseError> {
        let mut segments = 0usize;
        loop {
            let is_ident = matches!(self.peek_at(segments * 2).kind, TokenKind::Ident { .. });
            match (is_ident, &self.peek_at(segments * 2 + 1).kind) {
                (true, TokenKind::Dot) => segments += 1,
                (true, TokenKind::LParen) => {
                    return self.function_call(segments + 1);
                }
                _ => break,
            }
        }
        let (name, span) = self.binding_name("a variable", "a variable")?;
        Ok(Expr::Variable { name, span })
    }

    fn function_call(&mut self, segments: usize) -> Result<Expr, ParseError> {
        let start = self.peek().span;
        let mut name = Vec::with_capacity(segments);
        for i in 0..segments {
            name.push(self.any_name("a function name segment")?.0);
            if i < segments - 1 {
                self.expect(&TokenKind::Dot, "'.' in the function name")?;
            }
        }
        self.expect(&TokenKind::LParen, "'(' to open the call")?;
        let mut distinct = false;
        let mut star = false;
        let mut args = Vec::new();
        if self.at(&TokenKind::Star) {
            self.bump();
            star = true;
        } else if !self.at(&TokenKind::RParen) {
            distinct = self.eat_kw("DISTINCT");
            loop {
                args.push(self.expression()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let close = self.expect(&TokenKind::RParen, "')' to close the call")?;
        Ok(Expr::FunctionCall {
            name,
            distinct,
            args,
            star,
            span: start.to(close.span),
        })
    }

    fn case_expression(&mut self) -> Result<Expr, ParseError> {
        let start = self.expect_kw("CASE")?.span;
        let operand = if self.at_kw("WHEN") {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw("WHEN") {
            let condition = self.expression()?;
            self.expect_kw("THEN")?;
            let value = self.expression()?;
            whens.push((condition, value));
        }
        if whens.is_empty() {
            return Err(self.unexpected("WHEN after CASE"));
        }
        let else_expr = if self.eat_kw("ELSE") {
            Some(Box::new(self.expression()?))
        } else {
            None
        };
        let end = self.expect_kw("END")?.span;
        Ok(Expr::Case {
            operand,
            whens,
            else_expr,
            span: start.to(end),
        })
    }

    /// With the cursor on a quantifier keyword: do the next tokens open a
    /// quantifier body `( name IN ...`?
    fn at_quantifier_body(&self) -> bool {
        self.peek_at(1).kind == TokenKind::LParen
            && matches!(self.peek_at(2).kind, TokenKind::Ident { .. })
            && self.at_kw_at(3, "IN")
    }

    /// With the cursor on `reduce`: do the next tokens open `( name = `?
    fn at_reduce_body(&self) -> bool {
        self.peek_at(1).kind == TokenKind::LParen
            && matches!(self.peek_at(2).kind, TokenKind::Ident { .. })
            && self.peek_at(3).kind == TokenKind::Eq
    }

    /// `all|any|none|single ( variable IN list WHERE predicate )`.
    fn quantifier(&mut self, kind: QuantifierKind) -> Result<Expr, ParseError> {
        let start = self.bump().span; // the quantifier keyword
        self.expect(&TokenKind::LParen, "'(' to open the quantifier")?;
        let (variable, _) = self.binding_name("a quantifier variable", "a quantifier variable")?;
        self.expect_kw("IN")?;
        let list = Box::new(self.expression()?);
        self.expect_kw("WHERE")?;
        let predicate = Box::new(self.expression()?);
        let close = self.expect(&TokenKind::RParen, "')' to close the quantifier")?;
        Ok(Expr::Quantifier {
            kind,
            variable,
            list,
            predicate,
            span: start.to(close.span),
        })
    }

    /// `reduce ( accumulator = init , variable IN list | expr )`.
    fn reduce_expression(&mut self) -> Result<Expr, ParseError> {
        let start = self.bump().span; // `reduce`
        self.expect(&TokenKind::LParen, "'(' to open reduce")?;
        let (accumulator, _) = self.binding_name("an accumulator", "an accumulator")?;
        self.expect(&TokenKind::Eq, "'=' in reduce")?;
        let init = Box::new(self.expression()?);
        self.expect(&TokenKind::Comma, "',' in reduce")?;
        let (variable, _) = self.binding_name("a reduce variable", "a reduce variable")?;
        self.expect_kw("IN")?;
        let list = Box::new(self.expression()?);
        self.expect(&TokenKind::Pipe, "'|' in reduce")?;
        let expr = Box::new(self.expression()?);
        let close = self.expect(&TokenKind::RParen, "')' to close reduce")?;
        Ok(Expr::Reduce {
            accumulator,
            init,
            variable,
            list,
            expr,
            span: start.to(close.span),
        })
    }

    /// After seeing `[` at atom position: a list literal, or a list
    /// comprehension when the next tokens are `name IN`.
    fn list_literal_or_comprehension(&mut self) -> Result<Expr, ParseError> {
        let open = self.expect(&TokenKind::LBracket, "'['")?;
        let is_comprehension =
            matches!(self.peek().kind, TokenKind::Ident { .. }) && self.at_kw_at(1, "IN");
        if is_comprehension {
            let (variable, _) =
                self.binding_name("a comprehension variable", "a comprehension variable")?;
            self.expect_kw("IN")?;
            let list = Box::new(self.expression()?);
            let where_clause = if self.eat_kw("WHERE") {
                Some(Box::new(self.expression()?))
            } else {
                None
            };
            let map = if self.eat(&TokenKind::Pipe) {
                Some(Box::new(self.expression()?))
            } else {
                None
            };
            let close = self.expect(&TokenKind::RBracket, "']' to close the comprehension")?;
            return Ok(Expr::ListComprehension {
                variable,
                list,
                where_clause,
                map,
                span: open.span.to(close.span),
            });
        }
        let mut items = Vec::new();
        if !self.at(&TokenKind::RBracket) {
            loop {
                items.push(self.expression()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let close = self.expect(&TokenKind::RBracket, "']' to close the list")?;
        Ok(Expr::ListLiteral {
            items,
            span: open.span.to(close.span),
        })
    }

    fn map_literal(&mut self) -> Result<Expr, ParseError> {
        let open = self.expect(&TokenKind::LBrace, "'{' to open a map")?;
        let mut entries = Vec::new();
        if !self.at(&TokenKind::RBrace) {
            loop {
                let (key, _) = self.any_name("a map key")?;
                self.expect(&TokenKind::Colon, "':' after the map key")?;
                entries.push((key, self.expression()?));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let close = self.expect(&TokenKind::RBrace, "'}' to close the map")?;
        Ok(Expr::MapLiteral {
            entries,
            span: open.span.to(close.span),
        })
    }

    /// `(` at atom position opens either a parenthesised expression or a
    /// pattern predicate (`(h)-[:RUNS]->(:Software)`). Decided by linear
    /// token scans — NOT by speculative parsing, which is exponential on
    /// nested parentheses, and NOT by parse-then-fallback, which
    /// resurrects the same blow-up when every nesting level fails late
    /// and re-parses. We commit to a pattern only when the group is
    /// node-shaped AND a relationship connector follows; committed
    /// patterns that then fail to parse are hard errors.
    ///
    /// Known deliberate divergence (matches the Neo4j reading): a
    /// node-shaped group followed by a connector is always a pattern, so
    /// arithmetic that mimics one — `(a) - -(b)`, `(xs) - [0] - 1` — is
    /// rejected or read as a pattern. Subtraction with an unambiguous
    /// right-hand side (`(1) - -2`, `(1+2) - [0]`, `(1) - -(2+3)`) parses
    /// as arithmetic because the shape checks disambiguate it.
    fn paren_expr_or_pattern_predicate(&mut self) -> Result<Expr, ParseError> {
        if self.paren_group_starts_pattern() {
            let pattern = self.path_pattern()?;
            if pattern.steps.is_empty() {
                return Err(self.unexpected("a relationship pattern"));
            }
            let span = pattern.span;
            return Ok(Expr::PatternPredicate {
                pattern: Box::new(pattern),
                span,
            });
        }
        let open = self.expect(&TokenKind::LParen, "'('")?;
        let mut inner = self.expression()?;
        let close = self.expect(&TokenKind::RParen, "')' to close the expression")?;
        // The expression's span covers the parentheses, so derived
        // column names (`RETURN (x)`) render faithfully.
        inner.set_span(open.span.to(close.span));
        Ok(inner)
    }

    /// With the cursor on `(`: is this group a node pattern followed by a
    /// relationship connector? Linear in the distance to the close paren.
    fn paren_group_starts_pattern(&self) -> bool {
        debug_assert!(self.at(&TokenKind::LParen));
        let Some(close) = self.matching_close(0, &TokenKind::LParen, &TokenKind::RParen) else {
            return false;
        };
        self.connector_follows(close) && self.node_shaped_interior(close)
    }

    /// Offset of the token that closes the group opened at `offset`
    /// (tracking nesting of the same delimiter kind), or `None` if input
    /// ends first.
    fn matching_close(&self, offset: usize, open: &TokenKind, close: &TokenKind) -> Option<usize> {
        debug_assert!(&self.peek_at(offset).kind == open);
        let mut at = offset;
        let mut depth = 0usize;
        loop {
            let kind = &self.peek_at(at).kind;
            if kind == open {
                depth += 1;
            } else if kind == close {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            } else if kind == &TokenKind::Eof {
                return None;
            }
            at += 1;
        }
    }

    /// Does a relationship connector follow the group closing at `close`?
    /// `<-` always does; `-` only as `-->`, as `--` followed by `(`
    /// (undirected step into a node — a lone `- -` is double negation),
    /// or as `-[...]` whose bracket group is closed by `-` or `->`
    /// (relationship detail — otherwise it is subtraction of a list).
    fn connector_follows(&self, close: usize) -> bool {
        match &self.peek_at(close + 1).kind {
            TokenKind::LArrow => true,
            TokenKind::Minus => match &self.peek_at(close + 2).kind {
                TokenKind::Arrow => true,
                TokenKind::Minus => self.peek_at(close + 3).kind == TokenKind::LParen,
                TokenKind::LBracket => {
                    match self.matching_close(close + 2, &TokenKind::LBracket, &TokenKind::RBracket)
                    {
                        Some(bracket_close) => matches!(
                            &self.peek_at(bracket_close + 1).kind,
                            TokenKind::Minus | TokenKind::Arrow
                        ),
                        None => false,
                    }
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// Do the tokens inside the group closing at `close` match the node
    /// pattern grammar `[variable] (':' label)* [properties]`? A group
    /// like `(1+2)` cannot be a node, so a trailing `-`-shape after it is
    /// arithmetic, not a pattern.
    fn node_shaped_interior(&self, close: usize) -> bool {
        let mut at = 1usize;
        if matches!(self.peek_at(at).kind, TokenKind::Ident { .. }) {
            at += 1;
        }
        while self.peek_at(at).kind == TokenKind::Colon
            && matches!(self.peek_at(at + 1).kind, TokenKind::Ident { .. })
        {
            at += 2;
        }
        match &self.peek_at(at).kind {
            TokenKind::Parameter(_) => at += 1,
            TokenKind::LBrace => {
                match self.matching_close(at, &TokenKind::LBrace, &TokenKind::RBrace) {
                    Some(brace_close) => at = brace_close + 1,
                    None => return false,
                }
            }
            _ => {}
        }
        at == close
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(input: &str) -> Query {
        match parse(input) {
            Ok(q) => q,
            Err(e) => panic!("{input:?} should parse, got: {e}"),
        }
    }

    fn parse_err(input: &str) -> ParseError {
        match parse(input) {
            Ok(_) => panic!("{input:?} should not parse"),
            Err(e) => e,
        }
    }

    fn only_return_expr(query: &Query) -> &Expr {
        let Some(Clause::Return(p)) = query.clauses.last() else {
            panic!("expected a RETURN clause");
        };
        let ProjectionItem::Expr { expr, .. } = &p.items[0] else {
            panic!("expected an expression item");
        };
        expr
    }

    #[test]
    fn create_clause_can_terminate_a_query() {
        // A bare CREATE with no trailing RETURN is a complete query.
        let q = parse_ok("CREATE (a:Host {name: 'h1'})");
        assert!(matches!(q.clauses.last(), Some(Clause::Create(_))));

        // CREATE may precede RETURN and follow MATCH.
        let q = parse_ok("MATCH (a:Host) CREATE (a)-[:RUNS]->(s:Software) RETURN s");
        assert!(matches!(q.clauses[0], Clause::Match(_)));
        assert!(matches!(q.clauses[1], Clause::Create(_)));
        assert!(matches!(q.clauses[2], Clause::Return(_)));
    }

    #[test]
    fn create_relationship_pattern_parses() {
        let q = parse_ok("CREATE (a:A)-[r:R {w: 2}]->(b:B)");
        let Some(Clause::Create(c)) = q.clauses.last() else {
            panic!("expected CREATE");
        };
        let pattern = &c.patterns[0];
        assert_eq!(pattern.start.labels, vec!["A"]);
        assert_eq!(pattern.steps.len(), 1);
        assert_eq!(pattern.steps[0].0.direction, Direction::Out);
    }

    #[test]
    fn set_and_remove_clauses_parse() {
        let q = parse_ok("MATCH (n) SET n.x = 1, n = {a: 1}, n += {b: 2}, n:Label");
        let Some(Clause::Set(c)) = q.clauses.last() else {
            panic!("expected SET");
        };
        assert_eq!(c.items.len(), 4);
        assert!(matches!(c.items[0], SetItem::Property { .. }));
        assert!(matches!(c.items[1], SetItem::Replace { .. }));
        assert!(matches!(c.items[2], SetItem::Merge { .. }));
        assert!(matches!(c.items[3], SetItem::AddLabels { .. }));

        let q = parse_ok("MATCH (n) REMOVE n.x, n:Label");
        let Some(Clause::Remove(c)) = q.clauses.last() else {
            panic!("expected REMOVE");
        };
        assert_eq!(c.items.len(), 2);
        assert!(matches!(c.items[0], RemoveItem::Property { .. }));
        assert!(matches!(c.items[1], RemoveItem::Labels { .. }));
    }

    #[test]
    fn merge_clause_parses_with_on_actions() {
        let q = parse_ok("MERGE (a:N {k: 1}) ON CREATE SET a.x = 1 ON MATCH SET a.y = 2, a.z = 3");
        let Some(Clause::Merge(c)) = q.clauses.last() else {
            panic!("expected MERGE");
        };
        assert!(c.pattern.start.labels == vec!["N"]);
        assert_eq!(c.on_create.len(), 1);
        assert_eq!(c.on_match.len(), 2);

        // A bare MERGE terminates a query.
        let q = parse_ok("MERGE (a:N {k: 1})");
        assert!(matches!(q.clauses.last(), Some(Clause::Merge(_))));
    }

    #[test]
    fn delete_clauses_parse() {
        let q = parse_ok("MATCH (n) DELETE n");
        let Some(Clause::Delete(c)) = q.clauses.last() else {
            panic!("expected DELETE");
        };
        assert!(!c.detach);
        assert_eq!(c.targets.len(), 1);

        let q = parse_ok("MATCH (a)-[r]->(b) DETACH DELETE a, b");
        let Some(Clause::Delete(c)) = q.clauses.last() else {
            panic!("expected DETACH DELETE");
        };
        assert!(c.detach);
        assert_eq!(c.targets.len(), 2);
    }

    #[test]
    fn bare_match_is_not_a_complete_query() {
        // A non-write, non-RETURN terminal clause is still rejected.
        let err = parse_err("MATCH (n)");
        assert!(matches!(err, ParseError::QueryStructure { .. }));
    }

    #[test]
    fn precedence_add_mul() {
        let q = parse_ok("RETURN 1 + 2 * 3");
        let Expr::Binary {
            op: BinaryOp::Add,
            rhs,
            ..
        } = only_return_expr(&q)
        else {
            panic!("expected + at the top");
        };
        assert!(matches!(
            **rhs,
            Expr::Binary {
                op: BinaryOp::Mul,
                ..
            }
        ));
    }

    #[test]
    fn power_is_right_associative_over_unary_minus() {
        // openCypher grammar: unary minus is an operand of ^, so -2^2 is (-2)^2.
        let q = parse_ok("RETURN -2 ^ 2");
        let Expr::Binary {
            op: BinaryOp::Pow,
            lhs,
            ..
        } = only_return_expr(&q)
        else {
            panic!("expected ^ at the top");
        };
        assert!(matches!(
            **lhs,
            Expr::Unary {
                op: UnaryOp::Minus,
                ..
            }
        ));

        // Left-associative per openCypher (TCK Precedence2).
        let q = parse_ok("RETURN 2 ^ 3 ^ 4");
        let Expr::Binary {
            op: BinaryOp::Pow,
            lhs: pow_lhs,
            ..
        } = only_return_expr(&q)
        else {
            panic!("expected ^ at the top");
        };
        assert!(matches!(
            **pow_lhs,
            Expr::Binary {
                op: BinaryOp::Pow,
                ..
            }
        ));
    }

    #[test]
    fn boolean_layering() {
        let q = parse_ok("RETURN true OR false XOR true AND NOT false");
        let Expr::Binary {
            op: BinaryOp::Or,
            rhs,
            ..
        } = only_return_expr(&q)
        else {
            panic!("expected OR at the top");
        };
        let Expr::Binary {
            op: BinaryOp::Xor,
            rhs,
            ..
        } = &**rhs
        else {
            panic!("expected XOR under OR");
        };
        let Expr::Binary {
            op: BinaryOp::And,
            rhs,
            ..
        } = &**rhs
        else {
            panic!("expected AND under XOR");
        };
        assert!(matches!(
            &**rhs,
            Expr::Unary {
                op: UnaryOp::Not,
                ..
            }
        ));
    }

    #[test]
    fn quantifiers_and_reduce_parse() {
        // Quantifiers use list-predicate syntax, not function-call args.
        for (query, kind) in [
            ("RETURN all(x IN xs WHERE x > 0)", QuantifierKind::All),
            ("RETURN any(x IN xs WHERE x > 0)", QuantifierKind::Any),
            ("RETURN none(x IN xs WHERE x > 0)", QuantifierKind::None),
            ("RETURN single(x IN xs WHERE x > 0)", QuantifierKind::Single),
        ] {
            let q = parse_ok(query);
            let Expr::Quantifier { kind: k, .. } = only_return_expr(&q) else {
                panic!("expected a quantifier for {query}");
            };
            assert_eq!(*k, kind);
        }
        // reduce has its own accumulator grammar.
        let q = parse_ok("RETURN reduce(acc = 0, x IN xs | acc + x)");
        assert!(matches!(only_return_expr(&q), Expr::Reduce { .. }));

        // The same names remain callable as ordinary functions when not
        // followed by the quantifier body shape.
        let q = parse_ok("RETURN any([1, 2])");
        assert!(matches!(only_return_expr(&q), Expr::FunctionCall { .. }));
    }

    #[test]
    fn subtraction_vs_pattern() {
        let q = parse_ok("RETURN (1) - (2)");
        assert!(matches!(
            only_return_expr(&q),
            Expr::Binary {
                op: BinaryOp::Sub,
                ..
            }
        ));

        let q = parse_ok("MATCH (h) WHERE (h)--(x) RETURN h");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        assert!(matches!(
            m.where_clause,
            Some(Expr::PatternPredicate { .. })
        ));
    }

    /// Arithmetic whose surface mimics relationship connectors must stay
    /// arithmetic (PR #30 review finding 5): the shape checks look at
    /// both sides of the `-`.
    #[test]
    fn connector_lookalike_arithmetic_parses() {
        for query in [
            "RETURN (1) - -2",
            "RETURN (a.x) - -1",
            "RETURN (1) - -(2 + 3)",
            "RETURN (1 + 2) - [0]",
            "RETURN (xs) - [0]",
            "RETURN (1) * -2",
            "RETURN (1) + -2",
        ] {
            let q = parse_ok(query);
            assert!(
                !matches!(only_return_expr(&q), Expr::PatternPredicate { .. }),
                "parsed as pattern: {query}"
            );
        }
    }

    /// The deliberate residual divergence: a node-shaped group followed
    /// by a genuine connector shape always reads as a pattern, matching
    /// Neo4j. `(a) - -(b)` is therefore the undirected pattern
    /// `(a)--(b)`, not `a - (-b)`; and rel-detail-shaped brackets after a
    /// node-shaped group commit to a pattern even when their contents
    /// then fail to parse as one.
    #[test]
    fn node_shaped_group_with_connector_is_a_pattern() {
        let q = parse_ok("MATCH (n) WHERE (a) - -(b) RETURN n");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        assert!(matches!(
            m.where_clause,
            Some(Expr::PatternPredicate { .. })
        ));

        // Committed pattern whose bracket contents are not a relationship
        // detail: hard error, not silent reinterpretation as arithmetic.
        assert!(parse("RETURN (xs) - [0] - 1").is_err());
    }

    #[test]
    fn at_ref_forms() {
        let q = parse_ok("MATCH (n:Host) AT 'main~5' RETURN n");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        assert!(matches!(&m.at_ref, Some(AtRef::Refspec { value, .. }) if value == "main~5"));

        let q = parse_ok("MATCH (n) AT $ref WHERE n.x = 1 RETURN n");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        assert!(matches!(&m.at_ref, Some(AtRef::Parameter { name, .. }) if name == "ref"));
        assert!(m.where_clause.is_some());
    }

    #[test]
    fn at_bare_refspec_error_suggests_quoting() {
        // A digit-leading abbreviated commit hash lexes as integer + ident;
        // the error must reproduce the raw text and tell the user to quote.
        let e = parse_err("MATCH (n) AT 3db804f9 RETURN n");
        assert!(e.to_string().contains("AT '3db804f9'"), "{e}");

        // A full 40-hex-character hash.
        let e = parse_err("MATCH (n) AT 4cdc014851d5c50e2bfa9b3d4a2e1a2b3c4d5e6f RETURN n");
        assert!(
            e.to_string()
                .contains("AT '4cdc014851d5c50e2bfa9b3d4a2e1a2b3c4d5e6f'"),
            "{e}"
        );

        // A hash whose leading digits form a float-looking prefix (4e5...).
        let e = parse_err("MATCH (n) AT 4e5c014851 RETURN n");
        assert!(e.to_string().contains("AT '4e5c014851'"), "{e}");

        // An unquoted branch name gets the same suggestion.
        let e = parse_err("MATCH (n) AT main RETURN n");
        assert!(e.to_string().contains("AT 'main'"), "{e}");

        // Punctuated refspecs are reassembled from the adjacent-token run.
        let e = parse_err("MATCH (n) AT feature/foo-1.2 RETURN n");
        assert!(e.to_string().contains("AT 'feature/foo-1.2'"), "{e}");
    }

    #[test]
    fn at_without_refspec_keeps_the_generic_error() {
        // A reserved word after AT means the refspec is missing, not
        // unquoted — suggesting AT 'RETURN' would mislead.
        let e = parse_err("MATCH (n) AT RETURN n");
        assert!(
            e.to_string()
                .contains("a refspec string or parameter after AT"),
            "{e}"
        );
        let e = parse_err("MATCH (n) AT");
        assert!(
            e.to_string()
                .contains("a refspec string or parameter after AT"),
            "{e}"
        );
    }

    #[test]
    fn i64_min_literal_parses_in_all_bases() {
        for q in [
            "RETURN -9223372036854775808 AS literal",
            "RETURN -0x8000000000000000 AS literal",
            "RETURN -0o1000000000000000000000 AS literal",
            // Folding is token-level, so whitespace after '-' is allowed.
            "RETURN - 9223372036854775808 AS literal",
        ] {
            let query = parse_ok(q);
            let expr = only_return_expr(&query);
            assert!(
                matches!(
                    expr,
                    Expr::Literal {
                        value: Literal::Integer(n),
                        ..
                    } if *n == i64::MIN
                ),
                "{q} should fold to i64::MIN, got {expr:?}"
            );
        }
    }

    #[test]
    fn double_minus_folds_the_inner_minus_only() {
        // `--2^63` is -(i64::MIN): parses (one minus folds into the
        // literal), and the remaining negation overflows at runtime.
        let q = parse_ok("RETURN --9223372036854775808");
        let Expr::Unary {
            op: UnaryOp::Minus,
            operand,
            ..
        } = only_return_expr(&q)
        else {
            panic!("expected an outer unary minus");
        };
        assert!(matches!(
            **operand,
            Expr::Literal {
                value: Literal::Integer(i64::MIN),
                ..
            }
        ));
    }

    #[test]
    fn out_of_range_integers_error_cleanly() {
        for q in [
            // 2^63 with no unary minus to fold.
            "RETURN 9223372036854775808",
            "RETURN 0x8000000000000000",
            "RETURN 0o1000000000000000000000",
            // One below i64::MIN.
            "RETURN -9223372036854775809",
            "RETURN -0x8000000000000001",
            // Binary minus is subtraction, not literal negation.
            "RETURN 1-9223372036854775808",
            // u64::MAX negated.
            "RETURN -18446744073709551615",
        ] {
            let e = parse_err(q);
            assert!(e.to_string().contains("out of range"), "{q}: {e}");
        }
    }

    #[test]
    fn reserved_words_are_rejected_in_binding_positions() {
        assert!(matches!(
            parse_err("MATCH (return) RETURN 1"),
            ParseError::ReservedWord { .. }
        ));
        assert!(matches!(
            parse_err("MATCH (n) RETURN n.x AS match"),
            ParseError::ReservedWord { .. }
        ));
        assert!(matches!(
            parse_err("UNWIND [1] AS limit RETURN limit"),
            ParseError::ReservedWord { .. }
        ));
        // The spike's known false-accept: RETURN must not parse as a variable.
        assert!(parse("MATCH (n) WHERE n.x > RETURN n").is_err());
    }

    #[test]
    fn backquoting_lifts_reservation() {
        parse_ok("MATCH (`return`) RETURN `return`");
        parse_ok("MATCH (n) RETURN n.x AS `match`");
    }

    #[test]
    fn keywords_stay_legal_as_labels_types_and_property_names() {
        parse_ok("MATCH (n:Return)-[:CONTAINS]->(m) RETURN n.merge, m");
        parse_ok("RETURN {order: 1}.order");
    }

    #[test]
    fn clause_order_is_validated() {
        assert!(matches!(
            parse_err("MATCH (n)"),
            ParseError::QueryStructure { .. }
        ));
        assert!(matches!(
            parse_err("MATCH (n) RETURN n MATCH (m) RETURN m"),
            ParseError::QueryStructure { .. }
        ));
        assert!(matches!(parse_err(""), ParseError::QueryStructure { .. }));
        assert!(matches!(
            parse_err("   "),
            ParseError::QueryStructure { .. }
        ));
        // Standalone CALL needs no RETURN.
        parse_ok("CALL acetone.log('main')");
        // WITH cannot end a query.
        assert!(matches!(
            parse_err("MATCH (n) WITH n"),
            ParseError::QueryStructure { .. }
        ));
    }

    /// Recursion-guarded nesting: deep parens/lists error cleanly on a
    /// normal test stack. (MAX_DEPTH levels of debug-build parser frames
    /// legitimately need on the order of 1 MiB, so callers are expected to
    /// provide an ordinary >= 2 MiB stack; the guard's job is bounding
    /// depth *beyond* that.)
    #[test]
    fn depth_limit_is_an_error_not_a_stack_overflow() {
        let deep = format!("RETURN {}1{}", "(".repeat(10_000), ")".repeat(10_000));
        assert!(matches!(
            parse(&deep),
            Err(ParseError::RecursionLimit { .. })
        ));

        let deep_lists = format!("RETURN {}1{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(matches!(
            parse(&deep_lists),
            Err(ParseError::RecursionLimit { .. })
        ));
    }

    /// Loop-folded operator chains build deep ASTs with *constant*
    /// parse-time recursion; the post-parse depth check must reject them
    /// and — the point of the 128 KiB stack — tearing the transient deep
    /// AST down must not recurse either (iterative `Expr::drop`). Before
    /// that Drop was iterative, several of these aborted the process from
    /// inside a successful-looking parse.
    #[test]
    fn deep_operator_chains_are_rejected_and_dropped_on_a_tiny_stack() {
        let inputs = vec![
            format!("RETURN {} true", "NOT ".repeat(10_000)),
            format!("RETURN {}1", "-".repeat(10_000)),
            format!("RETURN 1{}", " + 1".repeat(10_000)),
            format!("RETURN 1{}", " ^ 1".repeat(10_000)),
            format!("RETURN true{}", " OR true".repeat(10_000)),
            format!("RETURN x{}", ".p".repeat(10_000)),
            format!("RETURN x{}", "[0]".repeat(10_000)),
            // Mixed precedence levels compounding on one spine (the shape
            // that defeated a per-level chain budget in review).
            format!("RETURN 1{}", " + 1 * -2 ^ x.p".repeat(5_000)),
        ];
        std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || {
                for pathological in &inputs {
                    assert!(
                        matches!(parse(pathological), Err(ParseError::RecursionLimit { .. })),
                        "expected RecursionLimit for: {}...",
                        &pathological[..40]
                    );
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// Regression for the exponential-backtracking DoS found in review: a
    /// ~176-byte query of nested parenthesised map values took 10 s under
    /// speculative pattern parsing (2^n). With the token-scan decision it
    /// is linear; at depth 500 an exponential parser would never return,
    /// so this test completing at all is the assertion.
    #[test]
    fn nested_paren_map_values_parse_in_linear_time() {
        let depth = 500;
        let query = format!("RETURN {}1{}", "({a: ".repeat(depth), "})".repeat(depth));
        // Two expression levels per textual level: exceeds MAX_DEPTH and
        // must fail fast with the recursion guard, not hang.
        assert!(matches!(
            parse(&query),
            Err(ParseError::RecursionLimit { .. })
        ));

        // A shallow instance of the same shape parses fine.
        let depth = 20;
        let query = format!("RETURN {}1{}", "({a: ".repeat(depth), "})".repeat(depth));
        parse_ok(&query);
    }

    #[test]
    fn depth_limit_leaves_reasonable_queries_alone() {
        let nested = format!("RETURN {}1{}", "(".repeat(60), ")".repeat(60));
        parse_ok(&nested);
        let chain = format!("RETURN 1{}", " + 1".repeat(200));
        parse_ok(&chain);
        let nots = format!("RETURN {} true", "NOT ".repeat(50));
        parse_ok(&nots);
    }

    /// The returned AST's depth is bounded by MAX_AST_DEPTH exactly: a
    /// chain just inside the bound parses, one just past it is rejected.
    #[test]
    fn ast_depth_bound_is_exact() {
        let ok = format!("RETURN 1{}", " + 1".repeat(MAX_AST_DEPTH - 1));
        let query = parse_ok(&ok);
        assert!(query.depth() <= MAX_AST_DEPTH);

        let too_deep = format!("RETURN 1{}", " + 1".repeat(MAX_AST_DEPTH));
        assert!(matches!(
            parse(&too_deep),
            Err(ParseError::RecursionLimit { limit, .. }) if limit == MAX_AST_DEPTH
        ));
    }

    #[test]
    fn var_length_bounds() {
        let q = parse_ok("MATCH (a)-[:R*2..5]->(b) RETURN a");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        let (rel, _) = &m.patterns[0].steps[0];
        assert_eq!(
            rel.var_length,
            Some(VarLength {
                min: Some(2),
                max: Some(5)
            })
        );

        let q = parse_ok("MATCH (a)-[*3]->(b) RETURN a");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        let (rel, _) = &m.patterns[0].steps[0];
        assert_eq!(
            rel.var_length,
            Some(VarLength {
                min: Some(3),
                max: Some(3)
            })
        );

        let q = parse_ok("MATCH (a)-[*..4]->(b) RETURN a");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        let (rel, _) = &m.patterns[0].steps[0];
        assert_eq!(
            rel.var_length,
            Some(VarLength {
                min: None,
                max: Some(4)
            })
        );

        assert!(parse("MATCH (a)-[*-1]->(b) RETURN a").is_err());
    }

    #[test]
    fn bracketless_relationships() {
        let q = parse_ok("MATCH (a)-->(b)<--(c)--(d) RETURN a");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        let dirs: Vec<Direction> = m.patterns[0]
            .steps
            .iter()
            .map(|(r, _)| r.direction)
            .collect();
        assert_eq!(
            dirs,
            vec![Direction::Out, Direction::In, Direction::Undirected]
        );
    }

    #[test]
    fn parameters_as_pattern_properties() {
        let q = parse_ok("MATCH (n:Host $props) RETURN n");
        let Some(Clause::Match(m)) = q.clauses.first() else {
            panic!()
        };
        assert!(matches!(
            m.patterns[0].start.properties,
            Some(Expr::Parameter { .. })
        ));
    }

    #[test]
    fn count_star_and_distinct_aggregates() {
        parse_ok("MATCH (n) RETURN count(*) AS n_total, count(DISTINCT n.name) AS names");
    }

    #[test]
    fn errors_carry_spans_and_render_line_col() {
        let source = "MATCH (n)\nRETURN n.";
        let err = parse_err(source);
        assert!(err.span().start >= 10);
        let rendered = err.render(source);
        assert!(rendered.starts_with("line 2"), "got: {rendered}");
    }

    #[test]
    fn is_null_forms() {
        let q = parse_ok("RETURN null IS NULL, 1 IS NOT NULL");
        let Some(Clause::Return(p)) = q.clauses.last() else {
            panic!()
        };
        assert_eq!(p.items.len(), 2);
        let ProjectionItem::Expr { expr, .. } = &p.items[1] else {
            panic!()
        };
        assert!(matches!(expr, Expr::IsNull { negated: true, .. }));
    }

    #[test]
    fn return_star_and_order_by() {
        parse_ok("MATCH (n) RETURN * ORDER BY n.name ASC, n.age DESCENDING SKIP 1 LIMIT 2");
    }

    #[test]
    fn with_where_then_return() {
        parse_ok("MATCH (v)<-[:S]-(s) WITH v, count(s) AS n WHERE n > 3 RETURN v.name, n");
    }

    #[test]
    fn trailing_semicolon_is_allowed() {
        parse_ok("RETURN 1;");
        assert!(parse("RETURN 1; RETURN 2").is_err());
    }
}
