//! The TCK's expected-value notation, as written in result tables:
//! integers, floats, single-quoted strings, booleans, null, lists and
//! maps. Graph-entity notation (`(:L {p: 1})`, `[:T]`, `<...>` paths) is
//! recognised but unsupported for now — scenarios expecting entities need
//! graph setup, which needs the Phase 3 write path anyway.

use acetone_cypher::exec::QueryResult;
use acetone_cypher::exec::value::Value;

/// A parsed expected table.
#[derive(Debug)]
pub struct ExpectedTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub ordered: bool,
}

#[derive(Debug, PartialEq)]
pub enum ExpectedError {
    /// Cell uses notation this comparator does not model yet.
    UnsupportedNotation(String),
    Malformed(String),
}

pub fn parse_table(
    header: &[String],
    rows: &[Vec<String>],
    ordered: bool,
) -> Result<ExpectedTable, ExpectedError> {
    let mut parsed_rows = Vec::new();
    for row in rows {
        let mut parsed = Vec::new();
        for cell in row {
            parsed.push(parse_value(unescape_cell(cell.trim()).as_str())?);
        }
        parsed_rows.push(parsed);
    }
    Ok(ExpectedTable {
        columns: header.to_vec(),
        rows: parsed_rows,
        ordered,
    })
}

/// Gherkin table cells escape `|` and `\` with a backslash (the corpus
/// notes this in Literals6); undo that layer before value parsing.
fn unescape_cell(cell: &str) -> String {
    let mut out = String::with_capacity(cell.len());
    let mut chars = cell.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('|') => out.push('|'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_value(text: &str) -> Result<Value, ExpectedError> {
    let mut parser = CellParser { text, at: 0 };
    let value = parser.value()?;
    parser.skip_ws();
    if parser.at != parser.text.len() {
        return Err(ExpectedError::Malformed(text.to_string()));
    }
    Ok(value)
}

struct CellParser<'a> {
    text: &'a str,
    at: usize,
}

impl CellParser<'_> {
    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    fn skip_ws(&mut self) {
        while self.rest().starts_with(' ') {
            self.at += 1;
        }
    }

    fn eat(&mut self, token: &str) -> bool {
        if self.rest().starts_with(token) {
            self.at += token.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, ExpectedError> {
        self.skip_ws();
        if self.rest().starts_with('(')
            || self.rest().starts_with("[:")
            || self.rest().starts_with('<')
        {
            return Err(ExpectedError::UnsupportedNotation(self.text.to_string()));
        }
        if self.eat("null") {
            return Ok(Value::Null);
        }
        if self.eat("true") {
            return Ok(Value::Bool(true));
        }
        if self.eat("false") {
            return Ok(Value::Bool(false));
        }
        if self.eat("NaN") {
            return Ok(Value::Float(f64::NAN));
        }
        if self.rest().starts_with('\'') {
            return self.string();
        }
        if self.rest().starts_with('[') {
            return self.list();
        }
        if self.rest().starts_with('{') {
            return self.map();
        }
        self.number()
    }

    fn string(&mut self) -> Result<Value, ExpectedError> {
        let opened = self.eat("'");
        debug_assert!(opened, "caller guarantees the opening token");
        let mut out = String::new();
        let mut chars = self.rest().char_indices();
        while let Some((offset, c)) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some((_, escaped)) => out.push(escaped),
                    None => return Err(ExpectedError::Malformed(self.text.to_string())),
                },
                '\'' => {
                    self.at += offset + 1;
                    return Ok(Value::String(out));
                }
                c => out.push(c),
            }
        }
        Err(ExpectedError::Malformed(self.text.to_string()))
    }

    fn list(&mut self) -> Result<Value, ExpectedError> {
        let opened = self.eat("[");
        debug_assert!(opened, "caller guarantees the opening token");
        let mut items = Vec::new();
        self.skip_ws();
        if self.eat("]") {
            return Ok(Value::List(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            if self.eat("]") {
                return Ok(Value::List(items));
            }
            if !self.eat(",") {
                return Err(ExpectedError::Malformed(self.text.to_string()));
            }
        }
    }

    fn map(&mut self) -> Result<Value, ExpectedError> {
        let opened = self.eat("{");
        debug_assert!(opened, "caller guarantees the opening token");
        let mut entries = std::collections::BTreeMap::new();
        self.skip_ws();
        if self.eat("}") {
            return Ok(Value::Map(entries));
        }
        loop {
            self.skip_ws();
            let rest = self.rest();
            let key_len = rest
                .find(':')
                .ok_or_else(|| ExpectedError::Malformed(self.text.to_string()))?;
            let key = rest[..key_len].trim().trim_matches('\'').to_string();
            self.at += key_len + 1;
            entries.insert(key, self.value()?);
            self.skip_ws();
            if self.eat("}") {
                return Ok(Value::Map(entries));
            }
            if !self.eat(",") {
                return Err(ExpectedError::Malformed(self.text.to_string()));
            }
        }
    }

    fn number(&mut self) -> Result<Value, ExpectedError> {
        let rest = self.rest();
        let len = rest
            .char_indices()
            .take_while(|(_, c)| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);
        let token = rest[..len].to_string();
        if token.is_empty() {
            return Err(ExpectedError::UnsupportedNotation(self.text.to_string()));
        }
        self.at += len;
        if token.contains(['.', 'e', 'E']) {
            token
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| ExpectedError::Malformed(self.text.to_string()))
        } else {
            token
                .parse::<i64>()
                .map(Value::Int)
                .map_err(|_| ExpectedError::Malformed(self.text.to_string()))
        }
    }
}

/// Strict structural equality for verification: integers are not floats,
/// NaN equals NaN, lists/maps recurse. (Looser than execution's `=`,
/// stricter than orderability equivalence — this is result *checking*.)
fn values_match(expected: &Value, actual: &Value) -> bool {
    use Value::*;
    match (expected, actual) {
        (Null, Null) => true,
        (Bool(a), Bool(b)) => a == b,
        (Int(a), Int(b)) => a == b,
        (Float(a), Float(b)) => (a.is_nan() && b.is_nan()) || a == b,
        (String(a), String(b)) => a == b,
        (List(a), List(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_match(x, y))
        }
        (Map(a), Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, x)| b.get(k).is_some_and(|y| values_match(x, y)))
        }
        _ => false,
    }
}

/// Compare an execution result against an expected table. `None` means
/// match; `Some(reason)` is the mismatch description.
pub fn compare(expected: &ExpectedTable, actual: &QueryResult) -> Option<String> {
    if expected.columns != actual.columns {
        return Some(format!(
            "columns differ: expected {:?}, got {:?}",
            expected.columns, actual.columns
        ));
    }
    if expected.rows.len() != actual.rows.len() {
        return Some(format!(
            "row count differs: expected {}, got {}",
            expected.rows.len(),
            actual.rows.len()
        ));
    }
    if expected.ordered {
        for (index, (want, got)) in expected.rows.iter().zip(&actual.rows).enumerate() {
            if !rows_match(want, got) {
                return Some(format!(
                    "row {index} differs: expected {want:?}, got {got:?}"
                ));
            }
        }
        None
    } else {
        // Multiset comparison.
        let mut remaining: Vec<&Vec<Value>> = actual.rows.iter().collect();
        for want in &expected.rows {
            match remaining.iter().position(|got| rows_match(want, got)) {
                Some(found) => {
                    remaining.remove(found);
                }
                None => return Some(format!("expected row not found: {want:?}")),
            }
        }
        None
    }
}

fn rows_match(expected: &[Value], actual: &[Value]) -> bool {
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual)
            .all(|(want, got)| values_match(want, got))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_scalar_notation() {
        assert!(matches!(parse_value("42"), Ok(Value::Int(42))));
        assert!(matches!(parse_value("-1.5"), Ok(Value::Float(x)) if x == -1.5));
        assert!(matches!(parse_value("'hi'"), Ok(Value::String(s)) if s == "hi"));
        assert!(matches!(parse_value("true"), Ok(Value::Bool(true))));
        assert!(matches!(parse_value("null"), Ok(Value::Null)));
        assert!(
            matches!(parse_value("[1, 'a', [true]]"), Ok(Value::List(items)) if items.len() == 3)
        );
        assert!(matches!(parse_value("{a: 1, b: 'x'}"), Ok(Value::Map(m)) if m.len() == 2));
    }

    #[test]
    fn entity_notation_is_flagged_unsupported() {
        assert!(matches!(
            parse_value("(:Label {p: 1})"),
            Err(ExpectedError::UnsupportedNotation(_))
        ));
        assert!(matches!(
            parse_value("[:T]"),
            Err(ExpectedError::UnsupportedNotation(_))
        ));
        assert!(matches!(
            parse_value("<(:A)-[:R]->(:B)>"),
            Err(ExpectedError::UnsupportedNotation(_))
        ));
    }

    #[test]
    fn integer_and_float_expectations_are_distinct() {
        assert!(values_match(&Value::Int(1), &Value::Int(1)));
        assert!(!values_match(&Value::Int(1), &Value::Float(1.0)));
        assert!(values_match(
            &Value::Float(f64::NAN),
            &Value::Float(f64::NAN)
        ));
    }
}
