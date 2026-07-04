//! Spanned lexer for the spike subset. Keywords are case-insensitive per
//! openCypher; identifiers preserve case.

use super::ast::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Identifiers, literals, parameters
    Ident(String),
    Integer(i64),
    Float(f64),
    Str(String),
    Parameter(String),
    // Punctuation
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
    RegexEq, // =~
    Arrow,   // ->
    LArrow,  // <-  (only when followed by pattern context; lexed as Lt+Minus pair below)
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

// The spike deliberately avoids external dependencies for the hand-rolled
// half so its footprint is honest (the real parser would use thiserror per
// workspace convention).
#[derive(Debug)]
pub enum LexError {
    UnexpectedChar { ch: char, at: usize },
    UnterminatedString { at: usize },
    InvalidNumber { at: usize },
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LexError::UnexpectedChar { ch, at } => {
                write!(f, "unexpected character '{ch}' at byte {at}")
            }
            LexError::UnterminatedString { at } => {
                write!(f, "unterminated string starting at byte {at}")
            }
            LexError::InvalidNumber { at } => write!(f, "invalid number at byte {at}"),
        }
    }
}

impl std::error::Error for LexError {}

pub fn lex(input: &str) -> Result<Vec<Token>, LexError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let start = i;
        let c = input[i..].chars().next().unwrap();

        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        // Comments: // to end of line, /* ... */
        if input[i..].starts_with("//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if input[i..].starts_with("/*") {
            match input[i + 2..].find("*/") {
                Some(off) => i += 2 + off + 2,
                None => i = bytes.len(),
            }
            continue;
        }

        let (kind, len) = match c {
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
                // Leading-dot floats (`.5`) are omitted from the spike
                // subset; the real parser would accept them.
                if input[i..].starts_with("..") {
                    (TokenKind::DotDot, 2)
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
                let (s, len) = lex_string(input, i, c)?;
                tokens.push(Token {
                    kind: TokenKind::Str(s),
                    span: Span {
                        start,
                        end: start + len,
                    },
                });
                i += len;
                continue;
            }
            '$' => {
                let rest = &input[i + 1..];
                let len: usize = rest
                    .chars()
                    .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                    .map(char::len_utf8)
                    .sum();
                if len == 0 {
                    return Err(LexError::UnexpectedChar { ch: '$', at: i });
                }
                let name = rest[..len].to_string();
                tokens.push(Token {
                    kind: TokenKind::Parameter(name),
                    span: Span {
                        start,
                        end: start + 1 + len,
                    },
                });
                i += 1 + len;
                continue;
            }
            '`' => {
                // Backquoted identifier
                match input[i + 1..].find('`') {
                    Some(off) => {
                        let name = input[i + 1..i + 1 + off].to_string();
                        tokens.push(Token {
                            kind: TokenKind::Ident(name),
                            span: Span {
                                start,
                                end: i + off + 2,
                            },
                        });
                        i += off + 2;
                        continue;
                    }
                    None => return Err(LexError::UnterminatedString { at: i }),
                }
            }
            d if d.is_ascii_digit() => {
                let (kind, len) = lex_number(input, i)?;
                tokens.push(Token {
                    kind,
                    span: Span {
                        start,
                        end: start + len,
                    },
                });
                i += len;
                continue;
            }
            a if a.is_alphabetic() || a == '_' => {
                let len: usize = input[i..]
                    .chars()
                    .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                    .map(char::len_utf8)
                    .sum();
                let word = input[i..i + len].to_string();
                tokens.push(Token {
                    kind: TokenKind::Ident(word),
                    span: Span {
                        start,
                        end: start + len,
                    },
                });
                i += len;
                continue;
            }
            other => return Err(LexError::UnexpectedChar { ch: other, at: i }),
        };

        tokens.push(Token {
            kind,
            span: Span {
                start,
                end: start + len,
            },
        });
        i += len;
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        span: Span { start: i, end: i },
    });
    Ok(tokens)
}

fn lex_string(input: &str, at: usize, quote: char) -> Result<(String, usize), LexError> {
    let mut out = String::new();
    let mut chars = input[at + 1..].char_indices();
    while let Some((off, c)) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some((_, esc)) => out.push(match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    other => other,
                }),
                None => return Err(LexError::UnterminatedString { at }),
            },
            c if c == quote => return Ok((out, at + 1 + off + c.len_utf8() - at)),
            c => out.push(c),
        }
    }
    Err(LexError::UnterminatedString { at })
}

fn lex_number(input: &str, at: usize) -> Result<(TokenKind, usize), LexError> {
    let rest = &input[at..];
    let mut len = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    let mut is_float = false;
    // A '.' followed by a digit continues the number; '..' is a range.
    if rest[len..].starts_with('.')
        && !rest[len..].starts_with("..")
        && rest[len + 1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    {
        is_float = true;
        len += 1;
        len += rest[len..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .count();
    }
    if rest[len..].starts_with(['e', 'E']) {
        let mut elen = 1;
        if rest[len + elen..].starts_with(['+', '-']) {
            elen += 1;
        }
        let digits = rest[len + elen..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits > 0 {
            is_float = true;
            len += elen + digits;
        }
    }
    let text = &rest[..len];
    if is_float {
        text.parse::<f64>()
            .map(|f| (TokenKind::Float(f), len))
            .map_err(|_| LexError::InvalidNumber { at })
    } else {
        text.parse::<i64>()
            .map(|n| (TokenKind::Integer(n), len))
            .map_err(|_| LexError::InvalidNumber { at })
    }
}
