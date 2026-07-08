//! The `export` subcommand: dump a graph version as per-label node tables and
//! per-type edge tables in CSV or JSON/NDJSON (spec §7, §9 — the seed of the
//! relational projection). The inverse of `import`: export → import into a
//! fresh repo reproduces identical map roots.
//!
//! Thin client: pure output formatting over a read snapshot, like
//! `query --format`. Node key properties are re-exposed under their declared
//! names (the record stores only non-key properties, spec §2/§3), so a node's
//! full identity survives the round-trip.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::NodeRecord;
use acetone_model::schema::SchemaEntry;
use anyhow::{Context, Result, bail};

use crate::output::outln;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Csv,
    Json,
    Ndjson,
}

impl Format {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "csv" => Format::Csv,
            "json" => Format::Json,
            "ndjson" => Format::Ndjson,
            other => bail!("unknown export format {other:?}"),
        })
    }
    fn ext(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Ndjson => "ndjson",
        }
    }
}

/// Run the `export` subcommand.
pub fn run(
    repo_path: &Path,
    format: &str,
    label: Option<&str>,
    edge: Option<&str>,
    out: Option<&Path>,
) -> Result<()> {
    let format = Format::parse(format)?;
    let repo = crate::commands::open(repo_path)?;
    let snapshot = repo.workspace_snapshot()?;
    let schema = snapshot.schema_entries()?;
    let key_names = key_names(&schema);
    let nodes = snapshot.nodes()?;
    let edges = snapshot.edges()?;

    match (label, edge) {
        (Some(_), Some(_)) => bail!("--label and --edge are mutually exclusive"),
        (Some(label), None) => {
            let table = node_table(&nodes, &key_names, label);
            write_table(&table, format, out)?;
            outln!("exported {} {} node(s)", table.rows.len(), label);
        }
        (None, Some(rtype)) => {
            let table = edge_table(&edges, rtype)?;
            write_table(&table, format, out)?;
            outln!("exported {} {} edge(s)", table.rows.len(), rtype);
        }
        (None, None) => {
            let dir = out.context("exporting the whole graph needs --out <dir>")?;
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating export directory {}", dir.display()))?;
            export_all(&nodes, &edges, &schema, &key_names, format, dir)?;
        }
    }
    Ok(())
}

/// Map each label to its declared key property names.
fn key_names(schema: &[SchemaEntry]) -> BTreeMap<String, Vec<String>> {
    schema
        .iter()
        .filter_map(|e| match e {
            SchemaEntry::Label { name, def } => Some((name.clone(), def.key().to_vec())),
            _ => None,
        })
        .collect()
}

/// A tabular result: ordered column names and string/typed cells per row.
struct Table {
    columns: Vec<String>,
    rows: Vec<BTreeMap<String, Value>>,
}

/// A node's full property map: re-exposed key properties (in key order) plus
/// the record's non-key properties.
fn node_properties(
    key: &NodeKey,
    record: &NodeRecord,
    key_names: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Value> {
    let mut props = record.properties().clone();
    if let Some(names) = key_names.get(key.label()) {
        for (name, value) in names.iter().zip(key.key()) {
            props.entry(name.clone()).or_insert_with(|| value.clone());
        }
    }
    props
}

/// Build the node table for one label: key columns first (declaration order),
/// then the union of non-key property names, sorted.
fn node_table(
    nodes: &[(NodeKey, NodeRecord)],
    key_names: &BTreeMap<String, Vec<String>>,
    label: &str,
) -> Table {
    let key_cols = key_names.get(label).cloned().unwrap_or_default();
    let mut non_key: BTreeSet<String> = BTreeSet::new();
    let mut rows = Vec::new();
    for (key, record) in nodes.iter().filter(|(k, _)| k.label() == label) {
        let props = node_properties(key, record, key_names);
        for name in props.keys() {
            if !key_cols.contains(name) {
                non_key.insert(name.clone());
            }
        }
        rows.push(props);
    }
    let mut columns = key_cols;
    columns.extend(non_key);
    Table { columns, rows }
}

/// Build the edge table for one relationship type. v0.1 supports single-column
/// endpoint keys: columns `src`, `dst`, an optional `disc`, then edge
/// properties (labels are supplied at import via `--from`/`--to`).
fn edge_table(
    edges: &[(EdgeKey, acetone_model::records::EdgeRecord)],
    rtype: &str,
) -> Result<Table> {
    let mut non_key: BTreeSet<String> = BTreeSet::new();
    let mut has_disc = false;
    let mut rows = Vec::new();
    for (key, record) in edges.iter().filter(|(k, _)| k.rtype() == rtype) {
        if key.src().key().len() != 1 || key.dst().key().len() != 1 {
            bail!(
                "exporting edges of type {rtype:?} needs single-column endpoint keys \
                 (composite-key edge export is not yet supported)"
            );
        }
        let mut row = BTreeMap::new();
        row.insert("src".to_owned(), key.src().key()[0].clone());
        row.insert("dst".to_owned(), key.dst().key()[0].clone());
        if !matches!(key.disc(), Value::Null) {
            row.insert("disc".to_owned(), key.disc().clone());
            has_disc = true;
        }
        for (name, value) in record.properties() {
            non_key.insert(name.clone());
            row.insert(name.clone(), value.clone());
        }
        rows.push(row);
    }
    let mut columns = vec!["src".to_owned(), "dst".to_owned()];
    if has_disc {
        columns.push("disc".to_owned());
    }
    columns.extend(non_key);
    Ok(Table { columns, rows })
}

/// Export every keyed label and relationship type into `dir`.
fn export_all(
    nodes: &[(NodeKey, NodeRecord)],
    edges: &[(EdgeKey, acetone_model::records::EdgeRecord)],
    schema: &[SchemaEntry],
    key_names: &BTreeMap<String, Vec<String>>,
    format: Format,
    dir: &Path,
) -> Result<()> {
    let mut labels: BTreeSet<String> = BTreeSet::new();
    let mut rtypes: BTreeSet<String> = BTreeSet::new();
    for entry in schema {
        match entry {
            SchemaEntry::Label { name, .. } => {
                labels.insert(name.clone());
            }
            SchemaEntry::RelType { name, .. } => {
                rtypes.insert(name.clone());
            }
            SchemaEntry::Index { .. } => {}
        }
    }
    // Also cover any labels/types present in the data but not declared.
    labels.extend(nodes.iter().map(|(k, _)| k.label().to_owned()));
    rtypes.extend(edges.iter().map(|(k, _)| k.rtype().to_owned()));

    for label in &labels {
        let table = node_table(nodes, key_names, label);
        let path = dir.join(format!("{label}.{}", format.ext()));
        write_table(&table, format, Some(&path))?;
        outln!("exported {} node(s) → {}", table.rows.len(), path.display());
    }
    for rtype in &rtypes {
        let table = edge_table(edges, rtype)?;
        let path = dir.join(format!("rel-{rtype}.{}", format.ext()));
        write_table(&table, format, Some(&path))?;
        outln!("exported {} edge(s) → {}", table.rows.len(), path.display());
    }
    Ok(())
}

/// Serialise a table and write it to `out` (a file) or stdout.
fn write_table(table: &Table, format: Format, out: Option<&Path>) -> Result<()> {
    let text = match format {
        Format::Csv => to_csv(table),
        Format::Json => to_json(table, false),
        Format::Ndjson => to_json(table, true),
    };
    match out {
        Some(path) => std::fs::write(path, text)
            .with_context(|| format!("writing export to {}", path.display()))?,
        None => outln!("{}", text.trim_end()),
    }
    Ok(())
}

fn to_csv(table: &Table) -> String {
    let mut out = String::new();
    out.push_str(
        &table
            .columns
            .iter()
            .map(|c| csv_field(c))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for row in &table.rows {
        let line = table
            .columns
            .iter()
            .map(|col| match row.get(col) {
                Some(v) => csv_field(&csv_cell(v)),
                None => String::new(),
            })
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

fn to_json(table: &Table, ndjson: bool) -> String {
    let objects: Vec<serde_json::Value> = table
        .rows
        .iter()
        .map(|row| {
            let map: serde_json::Map<String, serde_json::Value> = table
                .columns
                .iter()
                .filter_map(|col| row.get(col).map(|v| (col.clone(), json_of(v))))
                .collect();
            serde_json::Value::Object(map)
        })
        .collect();
    if ndjson {
        objects
            .iter()
            .map(|o| serde_json::to_string(o).expect("json"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        serde_json::to_string_pretty(&objects).expect("json")
    }
}

/// A CSV cell rendering of a scalar/list value (lists as JSON text). Bytes and
/// temporal values render as strings, matching the query representation.
fn csv_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => s.clone(),
        Value::Bytes(b) => hex(b),
        Value::List(_) => serde_json::to_string(&json_of(value)).unwrap_or_default(),
        other => format!("{other:?}"),
    }
}

/// A JSON rendering of a value (Bytes/temporal as strings; NaN as null).
fn json_of(value: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int(n) => J::Number((*n).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            .unwrap_or(J::Null),
        Value::String(s) => J::String(s.clone()),
        Value::Bytes(b) => J::String(hex(b)),
        Value::List(items) => J::Array(items.iter().map(json_of).collect()),
        other => J::String(format!("{other:?}")),
    }
}

/// Quote a CSV field when it contains a comma, quote, or newline (RFC 4180).
fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use acetone_model::schema::LabelDef;

    fn schema() -> Vec<SchemaEntry> {
        vec![SchemaEntry::Label {
            name: "Host".into(),
            def: LabelDef::new(vec!["name".into()], BTreeMap::new(), [], []).expect("label"),
        }]
    }

    fn host(name: &str, props: &[(&str, Value)]) -> (NodeKey, NodeRecord) {
        (
            NodeKey::new("Host", vec![Value::String(name.into())]).expect("key"),
            NodeRecord::new(
                [],
                props
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), v.clone()))
                    .collect(),
            ),
        )
    }

    #[test]
    fn node_table_puts_key_first_then_sorted_non_key() {
        let nodes = vec![
            host(
                "web1",
                &[
                    ("os", Value::String("linux".into())),
                    ("cores", Value::Int(8)),
                ],
            ),
            host("db1", &[("cores", Value::Int(16))]), // missing `os`
        ];
        let table = node_table(&nodes, &key_names(&schema()), "Host");
        assert_eq!(table.columns, vec!["name", "cores", "os"]);
        assert_eq!(table.rows.len(), 2);
        // The key value is re-exposed under its declared name.
        assert_eq!(
            table.rows[0].get("name"),
            Some(&Value::String("web1".into()))
        );
    }

    #[test]
    fn csv_serialises_header_and_quotes_special_fields() {
        let nodes = vec![host(
            "a,b",
            &[("note", Value::String("has \"quote\"".into()))],
        )];
        let table = node_table(&nodes, &key_names(&schema()), "Host");
        let csv = to_csv(&table);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "name,note");
        assert_eq!(lines[1], "\"a,b\",\"has \"\"quote\"\"\"");
    }

    #[test]
    fn ndjson_omits_absent_properties_and_preserves_types() {
        let nodes = vec![host(
            "web1",
            &[("cores", Value::Int(8)), ("up", Value::Bool(true))],
        )];
        let table = node_table(&nodes, &key_names(&schema()), "Host");
        let nd = to_json(&table, true);
        let value: serde_json::Value = serde_json::from_str(nd.lines().next().unwrap()).unwrap();
        assert_eq!(value["cores"], serde_json::json!(8));
        assert_eq!(value["up"], serde_json::json!(true));
        assert_eq!(value["name"], serde_json::json!("web1"));
        // A property this node lacks is absent, not null.
        assert!(value.get("os").is_none());
    }
}
