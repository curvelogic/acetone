//! The TCK's expected-value notation, as written in result tables:
//! integers, floats, single-quoted strings, booleans, null, lists, maps,
//! and graph entities — nodes `(:L {p: 1})`, relationships `[:T {p: 1}]`
//! and paths `<(:A)-[:T]->(:B)>` (acetone-cbl.2). Entities compare
//! *structurally* (labels, type, properties, and path orientation),
//! never by identity: the table's entities are patterns, not ids.

use std::collections::BTreeMap;

use acetone_cypher::exec::QueryResult;
use acetone_cypher::exec::value::{EntityId, NodeValue, PathValue, RelValue, Value};

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
        if self.rest().starts_with('<') {
            return self.path();
        }
        if self.rest().starts_with("[:") {
            return self.relationship();
        }
        if self.rest().starts_with('(') {
            return self.node().map(Value::Node);
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

    /// An identifier token: a label, relationship-type or property name.
    fn ident(&mut self) -> Result<String, ExpectedError> {
        let rest = self.rest();
        let len = rest
            .char_indices()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .map(|(i, c)| i + c.len_utf8())
            .last()
            .unwrap_or(0);
        if len == 0 {
            return Err(ExpectedError::Malformed(self.text.to_string()));
        }
        let token = rest[..len].to_string();
        self.at += len;
        Ok(token)
    }

    /// An optional `{...}` property map, empty when absent.
    fn optional_properties(&mut self) -> Result<BTreeMap<String, Value>, ExpectedError> {
        self.skip_ws();
        if !self.rest().starts_with('{') {
            return Ok(BTreeMap::new());
        }
        match self.map()? {
            Value::Map(map) => Ok(map),
            _ => unreachable!("map() returns a map"),
        }
    }

    /// Node notation: `()`, `(:A:B)`, `({p: 1})`, `(:A {p: 1})`. The
    /// placeholder id is never compared — entities match structurally.
    fn node(&mut self) -> Result<NodeValue, ExpectedError> {
        let opened = self.eat("(");
        debug_assert!(opened, "caller guarantees the opening token");
        let mut labels = Vec::new();
        self.skip_ws();
        while self.eat(":") {
            labels.push(self.ident()?);
        }
        let properties = self.optional_properties()?;
        self.skip_ws();
        if !self.eat(")") {
            return Err(ExpectedError::Malformed(self.text.to_string()));
        }
        Ok(NodeValue {
            id: EntityId::from_bytes(Vec::new()),
            labels,
            properties,
        })
    }

    /// Relationship notation: `[:T]`, `[:T {p: 1}]`. Start/end ids are
    /// placeholders; a bare relationship cell has no endpoints to check.
    fn relationship(&mut self) -> Result<Value, ExpectedError> {
        let opened = self.eat("[");
        debug_assert!(opened, "caller guarantees the opening token");
        if !self.eat(":") {
            return Err(ExpectedError::Malformed(self.text.to_string()));
        }
        let rel_type = self.ident()?;
        let properties = self.optional_properties()?;
        self.skip_ws();
        if !self.eat("]") {
            return Err(ExpectedError::Malformed(self.text.to_string()));
        }
        Ok(Value::Relationship(RelValue {
            id: EntityId::from_bytes(Vec::new()),
            rel_type,
            start: EntityId::from_bytes(Vec::new()),
            end: EntityId::from_bytes(Vec::new()),
            properties,
        }))
    }

    /// Path notation: `<(:A)-[:T]->(:B)<-[:U]-(:C)>`. Orientation is
    /// encoded through positional endpoint ids (`#0`, `#1`, ...), so the
    /// comparator can check each step's direction without real ids. An
    /// undirected step (`-[:T]-`) has no orientation to pin and is
    /// unsupported notation.
    fn path(&mut self) -> Result<Value, ExpectedError> {
        let opened = self.eat("<");
        debug_assert!(opened, "caller guarantees the opening token");
        self.skip_ws();
        if !self.rest().starts_with('(') {
            return Err(ExpectedError::Malformed(self.text.to_string()));
        }
        let positional = |index: usize| EntityId::from_bytes(format!("#{index}").into_bytes());
        let mut node = self.node()?;
        node.id = positional(0);
        let mut nodes = vec![node];
        let mut rels = Vec::new();
        loop {
            self.skip_ws();
            if self.eat(">") {
                return Ok(Value::Path(PathValue { nodes, rels }));
            }
            let incoming = if self.eat("<-") {
                true
            } else if self.eat("-") {
                false
            } else {
                return Err(ExpectedError::Malformed(self.text.to_string()));
            };
            self.skip_ws();
            if !self.rest().starts_with("[:") {
                return Err(ExpectedError::UnsupportedNotation(self.text.to_string()));
            }
            let Value::Relationship(mut rel) = self.relationship()? else {
                unreachable!("relationship() returns a rel");
            };
            self.skip_ws();
            let closed_outgoing = self.eat("->");
            let closed_plain = !closed_outgoing && self.eat("-");
            match (incoming, closed_outgoing, closed_plain) {
                (true, false, true) | (false, true, false) => {}
                // `-[:T]-` pins no orientation; `<-[:T]->` is not a path.
                _ => {
                    return Err(ExpectedError::UnsupportedNotation(self.text.to_string()));
                }
            }
            self.skip_ws();
            if !self.rest().starts_with('(') {
                return Err(ExpectedError::Malformed(self.text.to_string()));
            }
            let mut next = self.node()?;
            next.id = positional(nodes.len());
            let (here, there) = (nodes.len() - 1, nodes.len());
            if incoming {
                rel.start = positional(there);
                rel.end = positional(here);
            } else {
                rel.start = positional(here);
                rel.end = positional(there);
            }
            nodes.push(next);
            rels.push(rel);
        }
    }

    fn string(&mut self) -> Result<Value, ExpectedError> {
        let opened = self.eat("'");
        debug_assert!(opened, "caller guarantees the opening token");
        let mut out = String::new();
        let mut chars = self.rest().char_indices();
        while let Some((offset, c)) = chars.next() {
            match c {
                // The cell (after the Gherkin `\|`/`\\` layer) is a Cypher
                // string literal: mirror the lexer's escape set exactly.
                '\\' => match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, 'r')) => out.push('\r'),
                    Some((_, 'b')) => out.push('\u{0008}'),
                    Some((_, 'f')) => out.push('\u{000C}'),
                    Some((_, '\\')) => out.push('\\'),
                    Some((_, '\'')) => out.push('\''),
                    Some((_, '"')) => out.push('"'),
                    Some((_, esc @ ('u' | 'U'))) => {
                        let digits = if esc == 'u' { 4 } else { 8 };
                        let mut code = 0u32;
                        for _ in 0..digits {
                            let Some((_, hex)) = chars.next() else {
                                return Err(ExpectedError::Malformed(self.text.to_string()));
                            };
                            let Some(digit) = hex.to_digit(16) else {
                                return Err(ExpectedError::Malformed(self.text.to_string()));
                            };
                            code = code * 16 + digit;
                        }
                        let Some(ch) = char::from_u32(code) else {
                            return Err(ExpectedError::Malformed(self.text.to_string()));
                        };
                        out.push(ch);
                    }
                    _ => return Err(ExpectedError::Malformed(self.text.to_string())),
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
/// NaN equals NaN, lists/maps recurse. Entities compare structurally —
/// labels (as a set), relationship type, properties, and for paths each
/// step's orientation — never by id: the table's entities are patterns.
/// (Looser than execution's `=`, stricter than orderability equivalence —
/// this is result *checking*.)
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
        (Node(a), Node(b)) => nodes_match(a, b),
        (Relationship(a), Relationship(b)) => rels_match(a, b),
        (Path(a), Path(b)) => paths_match(a, b),
        _ => false,
    }
}

fn nodes_match(expected: &NodeValue, actual: &NodeValue) -> bool {
    use std::collections::BTreeSet;
    let want: BTreeSet<&String> = expected.labels.iter().collect();
    let got: BTreeSet<&String> = actual.labels.iter().collect();
    want == got && props_match(&expected.properties, &actual.properties)
}

/// Type and properties; endpoint identity is a path concern.
fn rels_match(expected: &RelValue, actual: &RelValue) -> bool {
    expected.rel_type == actual.rel_type && props_match(&expected.properties, &actual.properties)
}

fn props_match(expected: &BTreeMap<String, Value>, actual: &BTreeMap<String, Value>) -> bool {
    expected.len() == actual.len()
        && expected
            .iter()
            .all(|(k, x)| actual.get(k).is_some_and(|y| values_match(x, y)))
}

/// Paths match when their node/relationship sequences match pairwise and
/// every step points the same way. Orientation of step `i` is "forward"
/// when the relationship starts at node `i`; the expected side encodes it
/// through positional ids. A self-loop step (start == end) has both
/// orientations and matches either.
fn paths_match(expected: &PathValue, actual: &PathValue) -> bool {
    if expected.nodes.len() != actual.nodes.len() || expected.rels.len() != actual.rels.len() {
        return false;
    }
    if !expected
        .nodes
        .iter()
        .zip(&actual.nodes)
        .all(|(want, got)| nodes_match(want, got))
    {
        return false;
    }
    expected
        .rels
        .iter()
        .zip(&actual.rels)
        .enumerate()
        .all(|(i, (want, got))| {
            if !rels_match(want, got) {
                return false;
            }
            let forward = |path: &PathValue, rel: &RelValue| path.nodes[i].id.0 == rel.start.0;
            let backward = |path: &PathValue, rel: &RelValue| path.nodes[i + 1].id.0 == rel.start.0;
            (forward(expected, want) && forward(actual, got))
                || (backward(expected, want) && backward(actual, got))
        })
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
    fn entity_notation_parses() {
        let Ok(Value::Node(node)) = parse_value("(:Label {p: 1})") else {
            panic!("node notation must parse");
        };
        assert_eq!(node.labels, vec!["Label"]);
        assert!(matches!(node.properties.get("p"), Some(Value::Int(1))));

        assert!(matches!(parse_value("()"), Ok(Value::Node(n)) if n.labels.is_empty()));
        assert!(matches!(parse_value("(:A:B)"), Ok(Value::Node(n)) if n.labels == vec!["A", "B"]));

        let Ok(Value::Relationship(rel)) = parse_value("[:T {w: 'x'}]") else {
            panic!("relationship notation must parse");
        };
        assert_eq!(rel.rel_type, "T");
        assert!(matches!(rel.properties.get("w"), Some(Value::String(s)) if s == "x"));

        let Ok(Value::Path(path)) = parse_value("<(:A)-[:R]->(:B)<-[:S]-(:C)>") else {
            panic!("path notation must parse");
        };
        assert_eq!(path.nodes.len(), 3);
        assert_eq!(path.rels.len(), 2);
        // First step forward, second step backward.
        assert_eq!(path.rels[0].start.0, path.nodes[0].id.0);
        assert_eq!(path.rels[1].start.0, path.nodes[2].id.0);

        // A single-node path and a node inside a list both parse.
        assert!(matches!(parse_value("<()>"), Ok(Value::Path(p)) if p.rels.is_empty()));
        assert!(matches!(parse_value("[[:T], [:U]]"), Ok(Value::List(l)) if l.len() == 2));
    }

    #[test]
    fn string_cells_use_cypher_escapes() {
        assert!(matches!(
            parse_value(r"'\nFoo\n'"),
            Ok(Value::String(s)) if s == "\nFoo\n"
        ));
        assert!(matches!(
            parse_value(r"'\t\r\b\f'"),
            Ok(Value::String(s)) if s == "\t\r\u{0008}\u{000C}"
        ));
        assert!(matches!(
            parse_value(r"'é \U0001F600'"),
            Ok(Value::String(s)) if s == "\u{e9} \u{1F600}"
        ));
        // Unknown escapes are malformed, mirroring the lexer.
        assert!(matches!(
            parse_value(r"'\q'"),
            Err(ExpectedError::Malformed(_))
        ));
    }

    #[test]
    fn undirected_path_step_is_flagged_unsupported() {
        // `-[:T]-` pins no orientation to verify against.
        assert!(matches!(
            parse_value("<(:A)-[:R]-(:B)>"),
            Err(ExpectedError::UnsupportedNotation(_))
        ));
    }

    #[test]
    fn entity_matching_is_structural_never_by_id() {
        let make_node = |id: &str, labels: &[&str], key: Option<(&str, i64)>| {
            Value::Node(NodeValue {
                id: EntityId::from_bytes(id.as_bytes().to_vec()),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                properties: key
                    .map(|(k, v)| BTreeMap::from([(k.to_string(), Value::Int(v))]))
                    .unwrap_or_default(),
            })
        };
        let expected = parse_value("(:A:B {p: 1})").unwrap();
        // Different id, different label order: still a match.
        assert!(values_match(
            &expected,
            &make_node("real-id", &["B", "A"], Some(("p", 1)))
        ));
        // Extra property or missing label: no match.
        assert!(!values_match(
            &expected,
            &make_node("real-id", &["A"], Some(("p", 1)))
        ));
        assert!(!values_match(
            &expected,
            &make_node("real-id", &["A", "B"], None)
        ));
    }

    #[test]
    fn path_orientation_is_verified() {
        let node = |id: &str| NodeValue {
            id: EntityId::from_bytes(id.as_bytes().to_vec()),
            labels: vec!["A".to_string()],
            properties: BTreeMap::new(),
        };
        let rel = |start: &str, end: &str| RelValue {
            id: EntityId::from_bytes(Vec::new()),
            rel_type: "R".to_string(),
            start: EntityId::from_bytes(start.as_bytes().to_vec()),
            end: EntityId::from_bytes(end.as_bytes().to_vec()),
            properties: BTreeMap::new(),
        };
        let forward = Value::Path(PathValue {
            nodes: vec![node("n1"), node("n2")],
            rels: vec![rel("n1", "n2")],
        });
        let backward = Value::Path(PathValue {
            nodes: vec![node("n1"), node("n2")],
            rels: vec![rel("n2", "n1")],
        });
        let expect_forward = parse_value("<(:A)-[:R]->(:A)>").unwrap();
        let expect_backward = parse_value("<(:A)<-[:R]-(:A)>").unwrap();
        assert!(values_match(&expect_forward, &forward));
        assert!(!values_match(&expect_forward, &backward));
        assert!(values_match(&expect_backward, &backward));
        assert!(!values_match(&expect_backward, &forward));
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
