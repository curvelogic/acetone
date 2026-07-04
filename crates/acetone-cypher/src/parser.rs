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

/// Maximum AST depth including loop-folded operator chains
/// (`a + b + ...`, `NOT NOT ...`, `x.a.b...`), which nest the AST without
/// recursing at parse time. Bounding this protects `Drop` and every later
/// recursive walk (binder, planner) from adversarial chain lengths, while
/// staying loose enough for machine-generated queries with a few hundred
/// terms. Drop glue and walker frames are far smaller than parser frames,
/// hence the looser bound.
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
pub fn parse(input: &str) -> Result<Query, ParseError> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        depth: 0,
    };
    let query = parser.query()?;
    parser.expect_end()?;
    Ok(query)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
}

impl Parser {
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
            Some(Clause::Return(_)) | Some(Clause::Call(_)) => Ok(Query { clauses, span }),
            Some(other) => Err(ParseError::QueryStructure {
                message: "query must end with RETURN (or a standalone CALL)".into(),
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
        Err(self.unexpected("a clause (MATCH, OPTIONAL MATCH, UNWIND, WITH, RETURN or CALL)"))
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
                _ => return Err(self.unexpected("a refspec string or parameter after AT")),
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
    /// `x.a.b.c...`) also charge the same budget via [`Self::charge`]:
    /// they nest the AST even though parsing them does not recurse, and an
    /// unbounded AST is a stack overflow deferred to `Drop` or to any
    /// later recursive walk (binder, planner).
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

    /// Check that a chain of `links` additional AST levels on top of the
    /// current nesting stays within [`MAX_AST_DEPTH`].
    fn charge(&self, links: usize) -> Result<(), ParseError> {
        if self.depth.saturating_add(links) > MAX_AST_DEPTH {
            return Err(ParseError::RecursionLimit {
                limit: MAX_AST_DEPTH,
                span: self.peek().span,
            });
        }
        Ok(())
    }

    fn expr_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_xor()?;
        let mut links = 0usize;
        while self.eat_kw("OR") {
            links += 1;
            self.charge(links)?;
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
        let mut links = 0usize;
        while self.eat_kw("XOR") {
            links += 1;
            self.charge(links)?;
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
        let mut links = 0usize;
        while self.eat_kw("AND") {
            links += 1;
            self.charge(links)?;
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
            self.charge(nots.len())?;
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
        let mut lhs = self.expr_additive()?;
        let mut links = 0usize;
        loop {
            links += 1;
            self.charge(links)?;
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
            } else if self.eat(&TokenKind::RegexEq) {
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
        let mut links = 0usize;
        loop {
            links += 1;
            self.charge(links)?;
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
        let mut links = 0usize;
        loop {
            links += 1;
            self.charge(links)?;
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

    /// Right-associative `^`, folded iteratively so `1^1^1^...` cannot
    /// recurse.
    fn expr_power(&mut self) -> Result<Expr, ParseError> {
        let mut operands = vec![self.expr_unary()?];
        while self.eat(&TokenKind::Caret) {
            self.charge(operands.len())?;
            operands.push(self.expr_unary()?);
        }
        let mut expr = operands.pop().expect("at least one operand");
        while let Some(lhs) = operands.pop() {
            let span = lhs.span().to(expr.span());
            expr = Expr::Binary {
                op: BinaryOp::Pow,
                lhs: Box::new(lhs),
                rhs: Box::new(expr),
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
            self.charge(prefixes.len())?;
        }
        let mut expr = self.expr_postfix()?;
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
        let mut expr = self.expr_atom()?;
        let mut links = 0usize;
        loop {
            links += 1;
            self.charge(links)?;
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
    /// pattern predicate (`(h)-[:RUNS]->(:Software)`). Bounded
    /// backtracking: try a path pattern first and require at least one
    /// relationship step, otherwise rewind and parse an expression.
    fn paren_expr_or_pattern_predicate(&mut self) -> Result<Expr, ParseError> {
        let checkpoint = self.pos;
        if let Ok(pattern) = self.path_pattern()
            && !pattern.steps.is_empty()
        {
            let span = pattern.span;
            return Ok(Expr::PatternPredicate {
                pattern: Box::new(pattern),
                span,
            });
        }
        self.pos = checkpoint;
        self.expect(&TokenKind::LParen, "'('")?;
        let inner = self.expression()?;
        self.expect(&TokenKind::RParen, "')' to close the expression")?;
        Ok(inner)
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

        let q = parse_ok("RETURN 2 ^ 3 ^ 4");
        let Expr::Binary {
            op: BinaryOp::Pow,
            rhs,
            ..
        } = only_return_expr(&q)
        else {
            panic!("expected ^ at the top");
        };
        assert!(matches!(
            **rhs,
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

    #[test]
    fn depth_limit_is_an_error_not_a_stack_overflow() {
        // Parse-time recursion: nested parens and lists.
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

        // Loop-folded chains build AST depth without parse recursion; an
        // unbounded AST is a stack overflow deferred to Drop or to any
        // recursive walk (binder, planner), so chains charge the same
        // budget.
        for pathological in [
            format!("RETURN {} true", "NOT ".repeat(10_000)),
            format!("RETURN {}1", "-".repeat(10_000)),
            format!("RETURN 1{}", " + 1".repeat(10_000)),
            format!("RETURN 1{}", " ^ 1".repeat(10_000)),
            format!("RETURN true{}", " OR true".repeat(10_000)),
            format!("RETURN x{}", ".p".repeat(10_000)),
            format!("RETURN x{}", "[0]".repeat(10_000)),
        ] {
            assert!(
                matches!(parse(&pathological), Err(ParseError::RecursionLimit { .. })),
                "expected RecursionLimit for: {}...",
                &pathological[..40]
            );
        }
    }

    #[test]
    fn depth_limit_leaves_reasonable_queries_alone() {
        let nested = format!("RETURN {}1{}", "(".repeat(60), ")".repeat(60));
        parse_ok(&nested);
        let chain = format!("RETURN 1{}", " + 1".repeat(100));
        parse_ok(&chain);
        let nots = format!("RETURN {} true", "NOT ".repeat(50));
        parse_ok(&nots);
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
