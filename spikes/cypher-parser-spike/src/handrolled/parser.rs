//! Recursive-descent parser with Pratt expression parsing for the spike
//! subset, including the two classically awkward openCypher corners
//! (pattern predicates vs parenthesised expressions; list comprehensions
//! vs list literals) and the acetone `AT <ref>` extension.

use super::ast::*;
use super::lexer::{LexError, Token, TokenKind, lex};

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at bytes {}..{}",
            self.message, self.span.start, self.span.end
        )
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        let at = match e {
            LexError::UnexpectedChar { at, .. }
            | LexError::UnterminatedString { at }
            | LexError::InvalidNumber { at } => at,
        };
        ParseError {
            message: format!("lex error: {e:?}"),
            span: Span { start: at, end: at },
        }
    }
}

pub fn parse(input: &str) -> Result<Query, ParseError> {
    let tokens = lex(input)?;
    let mut p = Parser { tokens, pos: 0 };
    let query = p.query()?;
    p.expect_eof()?;
    Ok(query)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    // --- token helpers ---------------------------------------------------

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_at(&self, n: usize) -> &Token {
        &self.tokens[(self.pos + n).min(self.tokens.len() - 1)]
    }

    fn bump(&mut self) -> Token {
        let t = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
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

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<Token, ParseError> {
        if self.at(kind) {
            Ok(self.bump())
        } else {
            Err(self.error(format!("expected {what}, found {:?}", self.peek().kind)))
        }
    }

    fn error(&self, message: String) -> ParseError {
        ParseError {
            message,
            span: self.peek().span,
        }
    }

    /// Case-insensitive keyword check without consuming.
    fn at_kw(&self, kw: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Ident(w) if w.eq_ignore_ascii_case(kw))
    }

    fn at_kw2(&self, kw1: &str, kw2: &str) -> bool {
        self.at_kw(kw1)
            && matches!(&self.peek_at(1).kind, TokenKind::Ident(w) if w.eq_ignore_ascii_case(kw2))
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
            Err(self.error(format!("expected {kw}, found {:?}", self.peek().kind)))
        }
    }

    fn ident(&mut self, what: &str) -> Result<(String, Span), ParseError> {
        match self.peek().kind.clone() {
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                Ok((name, span))
            }
            other => Err(self.error(format!("expected {what}, found {other:?}"))),
        }
    }

    fn expect_eof(&mut self) -> Result<(), ParseError> {
        self.eat(&TokenKind::Semicolon);
        if self.at(&TokenKind::Eof) {
            Ok(())
        } else {
            Err(self.error(format!("unexpected trailing input: {:?}", self.peek().kind)))
        }
    }

    // --- query and clauses -------------------------------------------------

    fn query(&mut self) -> Result<Query, ParseError> {
        let start = self.peek().span;
        let mut clauses = Vec::new();
        loop {
            if self.at(&TokenKind::Eof) || self.at(&TokenKind::Semicolon) {
                break;
            }
            clauses.push(self.clause()?);
        }
        if clauses.is_empty() {
            return Err(self.error("empty query".into()));
        }
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Query {
            clauses,
            span: start.to(end),
        })
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
            let (alias, end) = self.ident("alias after AS")?;
            return Ok(Clause::Unwind {
                expr,
                alias,
                span: start.to(end),
            });
        }
        if self.at_kw("WITH") {
            let start = self.bump().span;
            let projection = self.projection(start, true)?;
            return Ok(Clause::With(projection));
        }
        if self.at_kw("RETURN") {
            let start = self.bump().span;
            let projection = self.projection(start, false)?;
            return Ok(Clause::Return(projection));
        }
        if self.at_kw("CALL") {
            return self.call_clause();
        }
        if self.at_kw("CREATE") {
            let start = self.bump().span;
            let patterns = self.pattern_list()?;
            let end = patterns.last().map(|p| p.span).unwrap_or(start);
            return Ok(Clause::Create {
                patterns,
                span: start.to(end),
            });
        }
        if self.at_kw("MERGE") {
            return self.merge_clause();
        }
        if self.at_kw("SET") {
            let start = self.bump().span;
            let items = self.set_items()?;
            let end = self.tokens[self.pos.saturating_sub(1)].span;
            return Ok(Clause::Set {
                items,
                span: start.to(end),
            });
        }
        if self.at_kw("REMOVE") {
            return self.remove_clause();
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
        Err(self.error(format!(
            "expected a clause keyword, found {:?}",
            self.peek().kind
        )))
    }

    fn match_clause(&mut self, optional: bool, start: Span) -> Result<Clause, ParseError> {
        let patterns = self.pattern_list()?;
        // Acetone extension (spec §5.2): `AT <refspec-string>` suffixes a
        // MATCH clause group. One token of lookahead, no grammar surgery —
        // this is the extensibility evidence for the hand-rolled option.
        let at_ref = if self.at_kw("AT") {
            self.bump();
            match self.peek().kind.clone() {
                TokenKind::Str(s) => {
                    let span = self.bump().span;
                    Some((s, span))
                }
                TokenKind::Parameter(name) => {
                    let span = self.bump().span;
                    Some((format!("${name}"), span))
                }
                _ => return Err(self.error("expected refspec string after AT".into())),
            }
        } else {
            None
        };
        let r#where = if self.eat_kw("WHERE") {
            Some(self.expression()?)
        } else {
            None
        };
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Clause::Match {
            optional,
            patterns,
            at_ref,
            r#where,
            span: start.to(end),
        })
    }

    fn projection(&mut self, start: Span, is_with: bool) -> Result<Projection, ParseError> {
        let distinct = self.eat_kw("DISTINCT");
        let mut items = Vec::new();
        loop {
            if self.eat(&TokenKind::Star) {
                items.push(ProjectionItem::Star);
            } else {
                let expr = self.expression()?;
                let alias = if self.eat_kw("AS") {
                    Some(self.ident("alias after AS")?.0)
                } else {
                    None
                };
                items.push(ProjectionItem::Expr { expr, alias });
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
                let order = if self.eat_kw("DESC") || self.eat_kw("DESCENDING") {
                    SortOrder::Desc
                } else {
                    self.eat_kw("ASC");
                    self.eat_kw("ASCENDING");
                    SortOrder::Asc
                };
                order_by.push((expr, order));
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
        let r#where = if is_with && self.eat_kw("WHERE") {
            Some(self.expression()?)
        } else {
            None
        };
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Projection {
            distinct,
            items,
            order_by,
            skip,
            limit,
            r#where,
            span: start.to(end),
        })
    }

    fn call_clause(&mut self) -> Result<Clause, ParseError> {
        let start = self.expect_kw("CALL")?.span;
        let mut procedure = vec![self.ident("procedure name")?.0];
        while self.eat(&TokenKind::Dot) {
            procedure.push(self.ident("procedure name segment")?.0);
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
            self.expect(&TokenKind::RParen, ") after procedure arguments")?;
        }
        let mut r#yield = Vec::new();
        if self.eat_kw("YIELD") {
            loop {
                r#yield.push(self.ident("yield column")?.0);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let r#where = if self.eat_kw("WHERE") {
            Some(self.expression()?)
        } else {
            None
        };
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Clause::Call {
            procedure,
            args,
            r#yield,
            r#where,
            span: start.to(end),
        })
    }

    fn merge_clause(&mut self) -> Result<Clause, ParseError> {
        let start = self.expect_kw("MERGE")?.span;
        let pattern = self.path_pattern()?;
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        while self.at_kw("ON") {
            self.bump();
            if self.eat_kw("CREATE") {
                self.expect_kw("SET")?;
                on_create.extend(self.set_items()?);
            } else if self.eat_kw("MATCH") {
                self.expect_kw("SET")?;
                on_match.extend(self.set_items()?);
            } else {
                return Err(self.error("expected CREATE or MATCH after ON".into()));
            }
        }
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Clause::Merge {
            pattern,
            on_create,
            on_match,
            span: start.to(end),
        })
    }

    fn set_items(&mut self) -> Result<Vec<SetItem>, ParseError> {
        let mut items = Vec::new();
        loop {
            // `var:Label` vs `expr.prop = expr`: a colon straight after the
            // variable means a label set.
            if matches!(self.peek().kind, TokenKind::Ident(_))
                && self.peek_at(1).kind == TokenKind::Colon
            {
                let (variable, _) = self.ident("variable")?;
                let mut labels = Vec::new();
                while self.eat(&TokenKind::Colon) {
                    labels.push(self.ident("label")?.0);
                }
                items.push(SetItem::Label { variable, labels });
            } else {
                // The target must be parsed as a property chain, not a full
                // expression — otherwise `h.x = 4` lexes `=` as equality.
                let target = self.property_target()?;
                self.expect(&TokenKind::Eq, "= in SET item")?;
                let value = self.expression()?;
                items.push(SetItem::Property { target, value });
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    /// `variable(.property)+` — the assignable subset of expressions.
    fn property_target(&mut self) -> Result<Expr, ParseError> {
        let (name, span) = self.ident("variable")?;
        let mut expr = Expr::Variable { name, span };
        while self.eat(&TokenKind::Dot) {
            let (key, end) = self.ident("property name after .")?;
            let span = expr.span().to(end);
            expr = Expr::Property {
                base: Box::new(expr),
                key,
                span,
            };
        }
        Ok(expr)
    }

    fn remove_clause(&mut self) -> Result<Clause, ParseError> {
        let start = self.expect_kw("REMOVE")?.span;
        let mut items = Vec::new();
        loop {
            if matches!(self.peek().kind, TokenKind::Ident(_))
                && self.peek_at(1).kind == TokenKind::Colon
            {
                let (variable, _) = self.ident("variable")?;
                let mut labels = Vec::new();
                while self.eat(&TokenKind::Colon) {
                    labels.push(self.ident("label")?.0);
                }
                items.push(RemoveItem::Label { variable, labels });
            } else {
                items.push(RemoveItem::Property(self.expression()?));
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Clause::Remove {
            items,
            span: start.to(end),
        })
    }

    fn delete_clause(&mut self, detach: bool, start: Span) -> Result<Clause, ParseError> {
        let mut exprs = Vec::new();
        loop {
            exprs.push(self.expression()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Clause::Delete {
            detach,
            exprs,
            span: start.to(end),
        })
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
        // Optional `p = ` path binding.
        let variable = if matches!(self.peek().kind, TokenKind::Ident(_))
            && self.peek_at(1).kind == TokenKind::Eq
        {
            let (name, _) = self.ident("path variable")?;
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
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(PathPattern {
            variable,
            start,
            steps,
            span: start_span.to(end),
        })
    }

    fn node_pattern(&mut self) -> Result<NodePattern, ParseError> {
        let open = self.expect(&TokenKind::LParen, "( to open node pattern")?;
        let variable = match self.peek().kind.clone() {
            TokenKind::Ident(name) => {
                self.bump();
                Some(name)
            }
            _ => None,
        };
        let mut labels = Vec::new();
        while self.eat(&TokenKind::Colon) {
            labels.push(self.ident("label")?.0);
        }
        let properties = if self.at(&TokenKind::LBrace) {
            Some(self.map_literal()?)
        } else {
            None
        };
        let close = self.expect(&TokenKind::RParen, ") to close node pattern")?;
        Ok(NodePattern {
            variable,
            labels,
            properties,
            span: open.span.to(close.span),
        })
    }

    fn rel_pattern(&mut self) -> Result<RelPattern, ParseError> {
        let start = self.peek().span;
        // <-[...]-  |  -[...]->  |  -[...]-  (and bracketless forms)
        let incoming = self.eat(&TokenKind::LArrow);
        if !incoming {
            self.expect(&TokenKind::Minus, "- to open relationship pattern")?;
        }

        let mut variable = None;
        let mut types = Vec::new();
        let mut var_length = None;
        let mut properties = None;

        if self.eat(&TokenKind::LBracket) {
            if let TokenKind::Ident(name) = self.peek().kind.clone() {
                self.bump();
                variable = Some(name);
            }
            if self.eat(&TokenKind::Colon) {
                types.push(self.ident("relationship type")?.0);
                while self.eat(&TokenKind::Pipe) {
                    self.eat(&TokenKind::Colon); // legacy `|:T` form
                    types.push(self.ident("relationship type")?.0);
                }
            }
            if self.eat(&TokenKind::Star) {
                var_length = Some(self.var_length()?);
            }
            if self.at(&TokenKind::LBrace) {
                properties = Some(self.map_literal()?);
            }
            self.expect(&TokenKind::RBracket, "] to close relationship detail")?;
        }

        let outgoing = if self.eat(&TokenKind::Arrow) {
            true
        } else {
            self.expect(&TokenKind::Minus, "- or -> to close relationship pattern")?;
            false
        };

        let direction = match (incoming, outgoing) {
            (true, false) => Direction::In,
            (false, true) => Direction::Out,
            (false, false) => Direction::Undirected,
            (true, true) => return Err(self.error("relationship cannot point both ways".into())),
        };
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(RelPattern {
            variable,
            types,
            direction,
            var_length,
            properties,
            span: start.to(end),
        })
    }

    fn var_length(&mut self) -> Result<VarLength, ParseError> {
        // Already consumed `*`. Forms: * | *n | *n..m | *n.. | *..m
        let mut vl = VarLength::default();
        if let TokenKind::Integer(n) = self.peek().kind {
            self.bump();
            vl.min = Some(n as u64);
            if self.eat(&TokenKind::DotDot) {
                if let TokenKind::Integer(m) = self.peek().kind {
                    self.bump();
                    vl.max = Some(m as u64);
                }
            } else {
                vl.max = vl.min; // exact length
            }
        } else if self.eat(&TokenKind::DotDot) {
            if let TokenKind::Integer(m) = self.peek().kind {
                self.bump();
                vl.max = Some(m as u64);
            }
        }
        Ok(vl)
    }

    // --- expressions ---------------------------------------------------------

    fn expression(&mut self) -> Result<Expr, ParseError> {
        self.expr_or()
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

    fn expr_not(&mut self) -> Result<Expr, ParseError> {
        if self.at_kw("NOT") {
            let start = self.bump().span;
            let operand = self.expr_not()?;
            let span = start.to(operand.span());
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                operand: Box::new(operand),
                span,
            });
        }
        self.expr_comparison()
    }

    fn expr_comparison(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.expr_additive()?;
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

    fn expr_power(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.expr_unary()?;
        if self.eat(&TokenKind::Caret) {
            let rhs = self.expr_power()?; // right associative
            let span = lhs.span().to(rhs.span());
            return Ok(Expr::Binary {
                op: BinaryOp::Pow,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            });
        }
        Ok(lhs)
    }

    fn expr_unary(&mut self) -> Result<Expr, ParseError> {
        if self.at(&TokenKind::Minus) {
            let start = self.bump().span;
            let operand = self.expr_unary()?;
            let span = start.to(operand.span());
            return Ok(Expr::Unary {
                op: UnaryOp::Minus,
                operand: Box::new(operand),
                span,
            });
        }
        if self.at(&TokenKind::Plus) {
            let start = self.bump().span;
            let operand = self.expr_unary()?;
            let span = start.to(operand.span());
            return Ok(Expr::Unary {
                op: UnaryOp::Plus,
                operand: Box::new(operand),
                span,
            });
        }
        self.expr_postfix()
    }

    fn expr_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.expr_atom()?;
        loop {
            if self.eat(&TokenKind::Dot) {
                let (key, end) = self.ident("property name after .")?;
                let span = expr.span().to(end);
                expr = Expr::Property {
                    base: Box::new(expr),
                    key,
                    span,
                };
            } else if self.at(&TokenKind::LBracket) {
                self.bump();
                // index `[e]`, or slice `[a..b]` / `[..b]` / `[a..]`
                if self.eat(&TokenKind::DotDot) {
                    let to = if self.at(&TokenKind::RBracket) {
                        None
                    } else {
                        Some(Box::new(self.expression()?))
                    };
                    let close = self.expect(&TokenKind::RBracket, "] to close slice")?;
                    let span = expr.span().to(close.span);
                    expr = Expr::Slice {
                        base: Box::new(expr),
                        from: None,
                        to,
                        span,
                    };
                } else {
                    let first = self.expression()?;
                    if self.eat(&TokenKind::DotDot) {
                        let to = if self.at(&TokenKind::RBracket) {
                            None
                        } else {
                            Some(Box::new(self.expression()?))
                        };
                        let close = self.expect(&TokenKind::RBracket, "] to close slice")?;
                        let span = expr.span().to(close.span);
                        expr = Expr::Slice {
                            base: Box::new(expr),
                            from: Some(Box::new(first)),
                            to,
                            span,
                        };
                    } else {
                        let close = self.expect(&TokenKind::RBracket, "] to close index")?;
                        let span = expr.span().to(close.span);
                        expr = Expr::Index {
                            base: Box::new(expr),
                            index: Box::new(first),
                            span,
                        };
                    }
                }
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
            } else if self.at_kw2("IS", "NOT")
                && matches!(&self.peek_at(2).kind, TokenKind::Ident(w) if w.eq_ignore_ascii_case("NULL"))
            {
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
            TokenKind::Str(ref s) => {
                self.bump();
                Ok(Expr::Literal {
                    value: Literal::String(s.clone()),
                    span: token.span,
                })
            }
            TokenKind::Parameter(ref name) => {
                self.bump();
                Ok(Expr::Parameter {
                    name: name.clone(),
                    span: token.span,
                })
            }
            TokenKind::LBracket => self.list_literal_or_comprehension(),
            TokenKind::LBrace => self.map_literal(),
            TokenKind::LParen => self.paren_expr_or_pattern_predicate(),
            TokenKind::Ident(ref word) => {
                if word.eq_ignore_ascii_case("null") {
                    self.bump();
                    return Ok(Expr::Literal {
                        value: Literal::Null,
                        span: token.span,
                    });
                }
                if word.eq_ignore_ascii_case("true") {
                    self.bump();
                    return Ok(Expr::Literal {
                        value: Literal::Boolean(true),
                        span: token.span,
                    });
                }
                if word.eq_ignore_ascii_case("false") {
                    self.bump();
                    return Ok(Expr::Literal {
                        value: Literal::Boolean(false),
                        span: token.span,
                    });
                }
                if word.eq_ignore_ascii_case("case") {
                    return self.case_expression();
                }
                self.variable_or_function_call()
            }
            ref other => Err(self.error(format!("expected an expression, found {other:?}"))),
        }
    }

    /// `name(...)`, `ns.name(...)`, `count(*)`, `collect(DISTINCT x)` or a
    /// plain variable reference. Dotted segments only become part of the
    /// function name when the chain ends in `(`; otherwise they are
    /// property accesses handled by the postfix loop.
    fn variable_or_function_call(&mut self) -> Result<Expr, ParseError> {
        // Count the dotted-ident chain length ending in `(`.
        let mut segments = 0usize;
        loop {
            match (
                &self.peek_at(segments * 2).kind,
                &self.peek_at(segments * 2 + 1).kind,
            ) {
                (TokenKind::Ident(_), TokenKind::Dot) => segments += 1,
                (TokenKind::Ident(_), TokenKind::LParen) => {
                    segments += 1;
                    return self.function_call(segments);
                }
                _ => break,
            }
        }
        let (name, span) = self.ident("variable")?;
        Ok(Expr::Variable { name, span })
    }

    fn function_call(&mut self, segments: usize) -> Result<Expr, ParseError> {
        let start = self.peek().span;
        let mut name = Vec::with_capacity(segments);
        for i in 0..segments {
            name.push(self.ident("function name segment")?.0);
            if i < segments - 1 {
                self.expect(&TokenKind::Dot, ". in function name")?;
            }
        }
        self.expect(&TokenKind::LParen, "( to open call")?;
        let mut distinct = false;
        let mut star = false;
        let mut args = Vec::new();
        if self.eat(&TokenKind::Star) {
            star = true; // count(*)
        } else if !self.at(&TokenKind::RParen) {
            distinct = self.eat_kw("DISTINCT");
            loop {
                args.push(self.expression()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let close = self.expect(&TokenKind::RParen, ") to close call")?;
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
            let cond = self.expression()?;
            self.expect_kw("THEN")?;
            let value = self.expression()?;
            whens.push((cond, value));
        }
        if whens.is_empty() {
            return Err(self.error("CASE requires at least one WHEN".into()));
        }
        let r#else = if self.eat_kw("ELSE") {
            Some(Box::new(self.expression()?))
        } else {
            None
        };
        let end = self.expect_kw("END")?.span;
        Ok(Expr::Case {
            operand,
            whens,
            r#else,
            span: start.to(end),
        })
    }

    /// `[1, 2, 3]` or `[x IN xs WHERE p | e]` — disambiguated by two tokens
    /// of lookahead (`Ident IN`).
    fn list_literal_or_comprehension(&mut self) -> Result<Expr, ParseError> {
        let open = self.expect(&TokenKind::LBracket, "[")?;
        let is_comprehension = matches!(&self.peek().kind, TokenKind::Ident(_))
            && matches!(&self.peek_at(1).kind, TokenKind::Ident(w) if w.eq_ignore_ascii_case("IN"));
        if is_comprehension {
            let (variable, _) = self.ident("comprehension variable")?;
            self.expect_kw("IN")?;
            let list = Box::new(self.expression()?);
            let r#where = if self.eat_kw("WHERE") {
                Some(Box::new(self.expression()?))
            } else {
                None
            };
            let map = if self.eat(&TokenKind::Pipe) {
                Some(Box::new(self.expression()?))
            } else {
                None
            };
            let close = self.expect(&TokenKind::RBracket, "] to close comprehension")?;
            return Ok(Expr::ListComprehension {
                variable,
                list,
                r#where,
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
        let close = self.expect(&TokenKind::RBracket, "] to close list")?;
        Ok(Expr::ListLiteral {
            items,
            span: open.span.to(close.span),
        })
    }

    fn map_literal(&mut self) -> Result<Expr, ParseError> {
        let open = self.expect(&TokenKind::LBrace, "{ to open map")?;
        let mut entries = Vec::new();
        if !self.at(&TokenKind::RBrace) {
            loop {
                let (key, _) = self.ident("map key")?;
                self.expect(&TokenKind::Colon, ": after map key")?;
                entries.push((key, self.expression()?));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let close = self.expect(&TokenKind::RBrace, "} to close map")?;
        Ok(Expr::MapLiteral {
            entries,
            span: open.span.to(close.span),
        })
    }

    /// `(` opens either a parenthesised expression or a pattern predicate
    /// (`(h)-[:RUNS]->(:Software)`). Resolved by bounded backtracking: try
    /// the pattern first and require at least one relationship step,
    /// otherwise rewind and parse as an expression.
    fn paren_expr_or_pattern_predicate(&mut self) -> Result<Expr, ParseError> {
        let checkpoint = self.pos;
        if let Ok(pattern) = self.path_pattern() {
            if !pattern.steps.is_empty() {
                let span = pattern.span;
                return Ok(Expr::PatternPredicate {
                    pattern: Box::new(pattern),
                    span,
                });
            }
        }
        self.pos = checkpoint;
        self.expect(&TokenKind::LParen, "(")?;
        let inner = self.expression()?;
        self.expect(&TokenKind::RParen, ") to close expression")?;
        Ok(inner)
    }
}
