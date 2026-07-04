//! Spanned lexer for openCypher. Keywords are not distinguished here —
//! they surface as `Ident` tokens and the parser decides contextually,
//! because openCypher keywords are unreserved in many positions (property
//! names, labels, map keys). Reserved-word enforcement lives in the
//! parser's binding positions.

use crate::error::ParseError;
use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// Identifier or keyword (case preserved). `backquoted: true` means the
    /// name was written `` `so quoted` `` and is never treated as a keyword.
    Ident {
        name: String,
        backquoted: bool,
    },
    Integer(i64),
    Float(f64),
    Str(String),
    /// `$name` or `$0`.
    Parameter(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Dot,
    DotDot,
    Colon,
    Semicolon,
    Pipe,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Caret,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `=~`
    RegexEq,
    /// `->`
    Arrow,
    /// `<-`
    LArrow,
    Eof,
}

impl TokenKind {
    /// Short human-readable description for error messages.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident { name, .. } => format!("'{name}'"),
            TokenKind::Integer(n) => format!("integer {n}"),
            TokenKind::Float(x) => format!("float {x}"),
            TokenKind::Str(_) => "string literal".into(),
            TokenKind::Parameter(name) => format!("parameter ${name}"),
            TokenKind::LParen => "'('".into(),
            TokenKind::RParen => "')'".into(),
            TokenKind::LBracket => "'['".into(),
            TokenKind::RBracket => "']'".into(),
            TokenKind::LBrace => "'{'".into(),
            TokenKind::RBrace => "'}'".into(),
            TokenKind::Comma => "','".into(),
            TokenKind::Dot => "'.'".into(),
            TokenKind::DotDot => "'..'".into(),
            TokenKind::Colon => "':'".into(),
            TokenKind::Semicolon => "';'".into(),
            TokenKind::Pipe => "'|'".into(),
            TokenKind::Star => "'*'".into(),
            TokenKind::Plus => "'+'".into(),
            TokenKind::Minus => "'-'".into(),
            TokenKind::Slash => "'/'".into(),
            TokenKind::Percent => "'%'".into(),
            TokenKind::Caret => "'^'".into(),
            TokenKind::Eq => "'='".into(),
            TokenKind::Ne => "'<>'".into(),
            TokenKind::Lt => "'<'".into(),
            TokenKind::Le => "'<='".into(),
            TokenKind::Gt => "'>'".into(),
            TokenKind::Ge => "'>='".into(),
            TokenKind::RegexEq => "'=~'".into(),
            TokenKind::Arrow => "'->'".into(),
            TokenKind::LArrow => "'<-'".into(),
            TokenKind::Eof => "end of input".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

fn lex_error(message: impl Into<String>, start: usize, end: usize) -> ParseError {
    ParseError::Lex {
        message: message.into(),
        span: Span::new(start, end),
    }
}

/// Tokenise the whole input. Always terminates with an `Eof` token; never
/// panics on any input.
pub fn lex(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let len = input.len();

    while i < len {
        let start = i;
        // `i` is always on a char boundary: it only advances by
        // `len_utf8()` of the char at `i` or by ASCII token lengths.
        let c = input[i..].chars().next().expect("i is on a char boundary");

        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if input[i..].starts_with("//") {
            i += input[i..].find('\n').unwrap_or(len - i);
            continue;
        }
        if input[i..].starts_with("/*") {
            match input[i + 2..].find("*/") {
                Some(off) => i += 2 + off + 2,
                None => return Err(lex_error("unterminated block comment", start, len)),
            }
            continue;
        }

        let (kind, width) = match c {
            '(' => (TokenKind::LParen, 1),
            ')' => (TokenKind::RParen, 1),
            '[' => (TokenKind::LBracket, 1),
            ']' => (TokenKind::RBracket, 1),
            '{' => (TokenKind::LBrace, 1),
            '}' => (TokenKind::RBrace, 1),
            ',' => (TokenKind::Comma, 1),
            ';' => (TokenKind::Semicolon, 1),
            '|' => (TokenKind::Pipe, 1),
            '*' => (TokenKind::Star, 1),
            '+' => (TokenKind::Plus, 1),
            '%' => (TokenKind::Percent, 1),
            '^' => (TokenKind::Caret, 1),
            '/' => (TokenKind::Slash, 1),
            ':' => (TokenKind::Colon, 1),
            '.' => {
                if input[i..].starts_with("..") {
                    (TokenKind::DotDot, 2)
                } else if next_is_ascii_digit(input, i + 1) {
                    // Leading-dot float: `.5`
                    let (kind, width) = lex_number(input, i)?;
                    tokens.push(Token {
                        kind,
                        span: Span::new(start, start + width),
                    });
                    i += width;
                    continue;
                } else {
                    (TokenKind::Dot, 1)
                }
            }
            '=' => {
                if input[i..].starts_with("=~") {
                    (TokenKind::RegexEq, 2)
                } else {
                    (TokenKind::Eq, 1)
                }
            }
            '<' => {
                if input[i..].starts_with("<=") {
                    (TokenKind::Le, 2)
                } else if input[i..].starts_with("<>") {
                    (TokenKind::Ne, 2)
                } else if input[i..].starts_with("<-") {
                    (TokenKind::LArrow, 2)
                } else {
                    (TokenKind::Lt, 1)
                }
            }
            '>' => {
                if input[i..].starts_with(">=") {
                    (TokenKind::Ge, 2)
                } else {
                    (TokenKind::Gt, 1)
                }
            }
            '-' => {
                if input[i..].starts_with("->") {
                    (TokenKind::Arrow, 2)
                } else {
                    (TokenKind::Minus, 1)
                }
            }
            '\'' | '"' => {
                let (value, width) = lex_string(input, i, c)?;
                tokens.push(Token {
                    kind: TokenKind::Str(value),
                    span: Span::new(start, start + width),
                });
                i += width;
                continue;
            }
            '$' => {
                let (name, width) = lex_parameter(input, i)?;
                tokens.push(Token {
                    kind: TokenKind::Parameter(name),
                    span: Span::new(start, start + width),
                });
                i += width;
                continue;
            }
            '`' => {
                let (name, width) = lex_backquoted(input, i)?;
                tokens.push(Token {
                    kind: TokenKind::Ident {
                        name,
                        backquoted: true,
                    },
                    span: Span::new(start, start + width),
                });
                i += width;
                continue;
            }
            d if d.is_ascii_digit() => {
                let (kind, width) = lex_number(input, i)?;
                tokens.push(Token {
                    kind,
                    span: Span::new(start, start + width),
                });
                i += width;
                continue;
            }
            a if a.is_alphabetic() || a == '_' => {
                let width = ident_len(&input[i..]);
                tokens.push(Token {
                    kind: TokenKind::Ident {
                        name: input[i..i + width].to_string(),
                        backquoted: false,
                    },
                    span: Span::new(start, start + width),
                });
                i += width;
                continue;
            }
            other => {
                return Err(lex_error(
                    format!("unexpected character '{}'", other.escape_default()),
                    start,
                    start + other.len_utf8(),
                ));
            }
        };

        tokens.push(Token {
            kind,
            span: Span::new(start, start + width),
        });
        i += width;
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        span: Span::new(len, len),
    });
    Ok(tokens)
}

fn next_is_ascii_digit(input: &str, at: usize) -> bool {
    input.as_bytes().get(at).is_some_and(u8::is_ascii_digit)
}

fn ident_len(rest: &str) -> usize {
    rest.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum()
}

/// `$name` or `$123`.
fn lex_parameter(input: &str, at: usize) -> Result<(String, usize), ParseError> {
    let rest = &input[at + 1..];
    let width = if rest.starts_with(|c: char| c.is_ascii_digit()) {
        rest.chars().take_while(char::is_ascii_digit).count()
    } else {
        ident_len(rest)
    };
    if width == 0 {
        return Err(lex_error("expected a parameter name after '$'", at, at + 1));
    }
    Ok((rest[..width].to_string(), 1 + width))
}

/// `` `name` `` with `` `` `` escaping a literal backquote.
fn lex_backquoted(input: &str, at: usize) -> Result<(String, usize), ParseError> {
    let mut name = String::new();
    let mut j = at + 1;
    loop {
        match input[j..].find('`') {
            None => return Err(lex_error("unterminated backquoted name", at, input.len())),
            Some(off) => {
                name.push_str(&input[j..j + off]);
                j += off + 1;
                if input[j..].starts_with('`') {
                    name.push('`');
                    j += 1;
                } else {
                    return Ok((name, j - at));
                }
            }
        }
    }
}

fn lex_string(input: &str, at: usize, quote: char) -> Result<(String, usize), ParseError> {
    let mut value = String::new();
    let mut chars = input[at + 1..].char_indices();
    while let Some((off, c)) = chars.next() {
        match c {
            '\\' => {
                let Some((esc_off, esc)) = chars.next() else {
                    return Err(lex_error("unterminated string literal", at, input.len()));
                };
                match esc {
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    'r' => value.push('\r'),
                    'b' => value.push('\u{0008}'),
                    'f' => value.push('\u{000C}'),
                    '\\' => value.push('\\'),
                    '\'' => value.push('\''),
                    '"' => value.push('"'),
                    'u' | 'U' => {
                        let digits = if esc == 'u' { 4 } else { 8 };
                        let hex_at = at + 1 + esc_off + 1;
                        let hex = input.get(hex_at..hex_at + digits).ok_or_else(|| {
                            lex_error("truncated unicode escape", at + 1 + off, input.len())
                        })?;
                        let code = u32::from_str_radix(hex, 16).map_err(|_| {
                            lex_error("invalid unicode escape", at + 1 + off, hex_at + digits)
                        })?;
                        let ch = char::from_u32(code).ok_or_else(|| {
                            lex_error(
                                format!("U+{code:04X} is not a valid character"),
                                at + 1 + off,
                                hex_at + digits,
                            )
                        })?;
                        value.push(ch);
                        // Skip the consumed hex digits (ASCII, one byte each).
                        for _ in 0..digits {
                            chars.next();
                        }
                    }
                    other => {
                        return Err(lex_error(
                            format!("unknown escape '\\{}'", other.escape_default()),
                            at + 1 + off,
                            at + 1 + esc_off + esc.len_utf8(),
                        ));
                    }
                }
            }
            c if c == quote => return Ok((value, off + 1 + c.len_utf8())),
            c => value.push(c),
        }
    }
    Err(lex_error("unterminated string literal", at, input.len()))
}

fn lex_number(input: &str, at: usize) -> Result<(TokenKind, usize), ParseError> {
    let rest = &input[at..];

    // Hex and octal integer forms.
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8)] {
        if let Some(digits) = rest.strip_prefix(prefix) {
            let width = digits
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .count();
            if width == 0 {
                return Err(lex_error("missing digits after numeric prefix", at, at + 2));
            }
            let value = i64::from_str_radix(&digits[..width], radix).map_err(|e| {
                lex_error(format!("invalid integer literal: {e}"), at, at + 2 + width)
            })?;
            return Ok((TokenKind::Integer(value), 2 + width));
        }
    }

    let mut width = rest.chars().take_while(char::is_ascii_digit).count();
    let mut is_float = false;

    // Fractional part. `1..2` must lex as `1` `..` `2`, so a lone `.` only
    // joins the number when a digit follows.
    if rest[width..].starts_with('.') && next_is_ascii_digit(rest, width + 1) {
        is_float = true;
        width += 1 + rest[width + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .count();
    }
    // Exponent.
    if rest[width..].starts_with(['e', 'E']) {
        let mut ewidth = 1;
        if rest[width + ewidth..].starts_with(['+', '-']) {
            ewidth += 1;
        }
        let digits = rest[width + ewidth..]
            .chars()
            .take_while(char::is_ascii_digit)
            .count();
        if digits > 0 {
            is_float = true;
            width += ewidth + digits;
        }
    }

    let text = &rest[..width];
    if is_float {
        // f64::parse saturates overflowing literals to infinity (`1e999`
        // lexes as `inf`); Neo4j/openCypher accept the same, so this is
        // deliberate rather than an error.
        match text.parse::<f64>() {
            Ok(x) => Ok((TokenKind::Float(x), width)),
            Err(e) => Err(lex_error(
                format!("invalid float literal: {e}"),
                at,
                at + width,
            )),
        }
    } else {
        match text.parse::<i64>() {
            Ok(n) => Ok((TokenKind::Integer(n), width)),
            Err(e) => Err(lex_error(
                format!("invalid integer literal: {e}"),
                at,
                at + width,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(input: &str) -> Vec<TokenKind> {
        lex(input).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn ident(name: &str) -> TokenKind {
        TokenKind::Ident {
            name: name.into(),
            backquoted: false,
        }
    }

    #[test]
    fn lexes_relationship_arrows() {
        assert_eq!(
            kinds("()-->()<--()--()"),
            vec![
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::Minus,
                TokenKind::Arrow,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LArrow,
                TokenKind::Minus,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::Minus,
                TokenKind::Minus,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_numbers() {
        assert_eq!(kinds("42"), vec![TokenKind::Integer(42), TokenKind::Eof]);
        assert_eq!(kinds("3.25"), vec![TokenKind::Float(3.25), TokenKind::Eof]);
        assert_eq!(kinds(".5"), vec![TokenKind::Float(0.5), TokenKind::Eof]);
        assert_eq!(kinds("1e3"), vec![TokenKind::Float(1000.0), TokenKind::Eof]);
        assert_eq!(
            kinds("2.5e-1"),
            vec![TokenKind::Float(0.25), TokenKind::Eof]
        );
        assert_eq!(kinds("0xff"), vec![TokenKind::Integer(255), TokenKind::Eof]);
        assert_eq!(kinds("0o17"), vec![TokenKind::Integer(15), TokenKind::Eof]);
        // Overflowing floats saturate to infinity, matching Neo4j.
        assert_eq!(
            kinds("1e999"),
            vec![TokenKind::Float(f64::INFINITY), TokenKind::Eof]
        );
    }

    #[test]
    fn range_after_integer_stays_a_range() {
        assert_eq!(
            kinds("1..3"),
            vec![
                TokenKind::Integer(1),
                TokenKind::DotDot,
                TokenKind::Integer(3),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn integer_overflow_is_an_error_not_a_panic() {
        assert!(matches!(
            lex("99999999999999999999"),
            Err(ParseError::Lex { .. })
        ));
    }

    #[test]
    fn lexes_strings_with_escapes() {
        assert_eq!(
            kinds(r#"'a\'b'"#),
            vec![TokenKind::Str("a'b".into()), TokenKind::Eof]
        );
        assert_eq!(
            kinds(r#""tab\there""#),
            vec![TokenKind::Str("tab\there".into()), TokenKind::Eof]
        );
        assert_eq!(
            kinds(r#"'é'"#),
            vec![TokenKind::Str("é".into()), TokenKind::Eof]
        );
        assert_eq!(
            kinds(r#"'\U0001F600'"#),
            vec![TokenKind::Str("😀".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn string_errors_carry_spans() {
        assert!(matches!(lex("'unterminated"), Err(ParseError::Lex { .. })));
        assert!(matches!(lex(r#"'\q'"#), Err(ParseError::Lex { .. })));
        assert!(matches!(lex(r#"'\u12'"#), Err(ParseError::Lex { .. })));
        assert!(matches!(lex(r#"'\uD800'"#), Err(ParseError::Lex { .. })));
    }

    #[test]
    fn lexes_parameters() {
        assert_eq!(
            kinds("$name"),
            vec![TokenKind::Parameter("name".into()), TokenKind::Eof]
        );
        assert_eq!(
            kinds("$0"),
            vec![TokenKind::Parameter("0".into()), TokenKind::Eof]
        );
        assert!(matches!(lex("$ x"), Err(ParseError::Lex { .. })));
    }

    #[test]
    fn lexes_backquoted_names_with_escapes() {
        assert_eq!(
            kinds("`a b`"),
            vec![
                TokenKind::Ident {
                    name: "a b".into(),
                    backquoted: true
                },
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("`a``b`"),
            vec![
                TokenKind::Ident {
                    name: "a`b".into(),
                    backquoted: true
                },
                TokenKind::Eof
            ]
        );
        assert!(matches!(lex("`open"), Err(ParseError::Lex { .. })));
    }

    #[test]
    fn comments_are_skipped_and_unterminated_block_comment_errors() {
        assert_eq!(
            kinds("1 // trailing\n2"),
            vec![TokenKind::Integer(1), TokenKind::Integer(2), TokenKind::Eof]
        );
        assert_eq!(
            kinds("1 /* mid */ 2"),
            vec![TokenKind::Integer(1), TokenKind::Integer(2), TokenKind::Eof]
        );
        assert!(matches!(lex("1 /* open"), Err(ParseError::Lex { .. })));
    }

    #[test]
    fn unicode_identifiers_lex() {
        assert_eq!(kinds("häuser"), vec![ident("häuser"), TokenKind::Eof]);
    }

    #[test]
    fn spans_are_byte_accurate() {
        let tokens = lex("MATCH (n)").unwrap();
        assert_eq!(tokens[0].span, Span::new(0, 5));
        assert_eq!(tokens[1].span, Span::new(6, 7));
        assert_eq!(tokens[2].span, Span::new(7, 8));
        assert_eq!(tokens[3].span, Span::new(8, 9));
    }
}
