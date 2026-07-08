//! The `import` subcommand: built-in CSV and JSON/NDJSON extractors, source
//! hashing, and the thin wiring to `acetone_graph::import` (spec §7,
//! ADR-0021). The orchestration, transform and provenance live in the graph
//! crate; this module only turns a file plus mapping flags into a
//! [`SourceExtractor`] and reports the outcome.

use std::collections::BTreeMap;
use std::path::Path;

use acetone_graph::import::{
    EndpointRef, ImportError, ImportOptions, ImportOutcome, ImportRecord, Provenance,
    SourceExtractor,
};
use acetone_model::Value;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::output::outln;

/// How source rows map to canonical records.
#[derive(Debug, Clone)]
pub enum Mapping {
    /// Each row is a node of `label`; every field is a property.
    Node { label: String },
    /// Each row is an edge of `rtype`; the endpoint fields carry the endpoint
    /// keys, the discriminator field (if any) the discriminator, and every
    /// remaining field is an edge property.
    Edge {
        rtype: String,
        from: EndpointSpec,
        to: EndpointSpec,
        disc: Option<String>,
    },
}

/// An endpoint mapping: a label and the fields carrying its key, in key order.
#[derive(Debug, Clone)]
pub struct EndpointSpec {
    pub label: String,
    pub fields: Vec<String>,
}

impl EndpointSpec {
    /// Parse `LABEL=field[,field...]`.
    pub fn parse(spec: &str) -> Result<Self> {
        let (label, fields) = spec
            .split_once('=')
            .with_context(|| format!("endpoint {spec:?} must be LABEL=field[,field...]"))?;
        if label.is_empty() {
            bail!("endpoint {spec:?} has an empty label");
        }
        let fields: Vec<String> = fields
            .split(',')
            .map(|f| f.trim().to_owned())
            .filter(|f| !f.is_empty())
            .collect();
        if fields.is_empty() {
            bail!("endpoint {spec:?} names no key fields");
        }
        Ok(EndpointSpec {
            label: label.to_owned(),
            fields,
        })
    }
}

/// A parsed source row: field name → value.
type Row = BTreeMap<String, Value>;

/// A built-in file extractor: parse the bytes into rows, then apply the
/// mapping. The parse is format-specific; the mapping is not.
struct FileExtractor {
    format: Format,
    bytes: Vec<u8>,
    mapping: Mapping,
}

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
            other => bail!("unknown import format {other:?}"),
        })
    }
    fn as_str(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Json => "json",
            Format::Ndjson => "ndjson",
        }
    }
}

impl SourceExtractor for FileExtractor {
    fn name(&self) -> &str {
        self.format.as_str()
    }

    fn extract(&mut self) -> Result<Vec<ImportRecord>, ImportError> {
        let rows = match self.format {
            Format::Csv => parse_csv(&self.bytes)?,
            Format::Json => parse_json(&self.bytes)?,
            Format::Ndjson => parse_ndjson(&self.bytes)?,
        };
        rows.into_iter()
            .map(|row| map_row(row, &self.mapping))
            .collect()
    }
}

/// Parse CSV with a header row; every cell is a string value.
fn parse_csv(bytes: &[u8]) -> Result<Vec<Row>, ImportError> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(bytes);
    let headers = reader
        .headers()
        .map_err(|e| ImportError::Extract(format!("reading CSV header: {e}")))?
        .clone();
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| ImportError::Extract(format!("reading CSV row: {e}")))?;
        let mut row = Row::new();
        for (name, value) in headers.iter().zip(record.iter()) {
            row.insert(name.to_owned(), Value::String(value.to_owned()));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Parse a JSON array of objects.
fn parse_json(bytes: &[u8]) -> Result<Vec<Row>, ImportError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| ImportError::Extract(format!("parsing JSON: {e}")))?;
    let array = value
        .as_array()
        .ok_or_else(|| ImportError::Extract("JSON import expects an array of objects".into()))?;
    array.iter().map(json_object_to_row).collect()
}

/// Parse newline-delimited JSON objects (one per non-blank line).
fn parse_ndjson(bytes: &[u8]) -> Result<Vec<Row>, ImportError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ImportError::Extract(format!("NDJSON is not valid UTF-8: {e}")))?;
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| ImportError::Extract(format!("parsing NDJSON line {}: {e}", i + 1)))?;
        rows.push(json_object_to_row(&value)?);
    }
    Ok(rows)
}

/// Convert a JSON object to a row, rejecting non-objects and nested objects.
fn json_object_to_row(value: &serde_json::Value) -> Result<Row, ImportError> {
    let object = value
        .as_object()
        .ok_or_else(|| ImportError::Extract("expected a JSON object".into()))?;
    let mut row = Row::new();
    for (name, value) in object {
        row.insert(name.clone(), json_to_value(value)?);
    }
    Ok(row)
}

/// Convert a JSON scalar (or list of scalars) to an acetone [`Value`]. Nested
/// objects are excluded from the v0.1 data model (spec §2).
fn json_to_value(value: &serde_json::Value) -> Result<Value, ImportError> {
    Ok(match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                return Err(ImportError::Extract(format!(
                    "number {n} is out of the supported i64/f64 range"
                )));
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(items) => {
            let mut list = Vec::with_capacity(items.len());
            for item in items {
                if item.is_array() || item.is_object() {
                    return Err(ImportError::Extract(
                        "nested lists and objects are not supported (spec §2)".into(),
                    ));
                }
                list.push(json_to_value(item)?);
            }
            Value::List(list)
        }
        serde_json::Value::Object(_) => {
            return Err(ImportError::Extract(
                "nested objects are not supported (spec §2)".into(),
            ));
        }
    })
}

/// Apply the mapping to one row.
fn map_row(row: Row, mapping: &Mapping) -> Result<ImportRecord, ImportError> {
    match mapping {
        Mapping::Node { label } => Ok(ImportRecord::Node {
            label: label.clone(),
            properties: row,
        }),
        Mapping::Edge {
            rtype,
            from,
            to,
            disc,
        } => {
            let mut row = row;
            let src = take_endpoint(&mut row, from)?;
            let dst = take_endpoint(&mut row, to)?;
            let discriminator = match disc {
                Some(field) => row.remove(field).unwrap_or(Value::Null),
                None => Value::Null,
            };
            Ok(ImportRecord::Edge {
                rtype: rtype.clone(),
                src,
                dst,
                discriminator,
                properties: row,
            })
        }
    }
}

/// Pull an endpoint's key values out of the row (consuming those fields, so
/// they do not also become edge properties).
fn take_endpoint(row: &mut Row, spec: &EndpointSpec) -> Result<EndpointRef, ImportError> {
    let mut key = Vec::with_capacity(spec.fields.len());
    for field in &spec.fields {
        let value = row.remove(field).ok_or_else(|| {
            ImportError::Mapping(format!(
                "edge row is missing endpoint key field {field:?} for label {:?}",
                spec.label
            ))
        })?;
        key.push(value);
    }
    Ok(EndpointRef {
        label: spec.label.clone(),
        key,
    })
}

/// SHA-256 of the raw source bytes, lower-case hex — a git-object-format
/// independent provenance hash (ADR-0021).
fn source_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Run the `import` subcommand.
#[allow(clippy::too_many_arguments)]
pub fn run(
    repo_path: &Path,
    format: &str,
    source: &Path,
    label: Option<&str>,
    edge: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    disc: Option<&str>,
    branch: Option<&str>,
    message: Option<&str>,
) -> Result<()> {
    let format = Format::parse(format)?;
    let mapping = build_mapping(label, edge, from, to, disc)?;

    let bytes = std::fs::read(source)
        .with_context(|| format!("reading import source {}", source.display()))?;
    let hash = source_hash(&bytes);

    let mut extractor = FileExtractor {
        format,
        bytes,
        mapping,
    };

    let repo = crate::commands::open(repo_path)?;
    let opts = ImportOptions {
        branch: branch.map(str::to_owned),
        message: message.map(str::to_owned),
        provenance: Provenance {
            source: source.display().to_string(),
            extractor: format.as_str().to_owned(),
            source_hash: hash,
        },
        author: None,
    };

    let outcome = acetone_graph::import(&repo, &mut extractor, opts).context("importing")?;

    let target = branch.unwrap_or("the current branch");
    match outcome {
        ImportOutcome::NoChange => {
            outln!("source unchanged; nothing imported");
        }
        ImportOutcome::Committed {
            commit,
            nodes,
            edges,
        } => {
            outln!(
                "imported {nodes} node(s) and {edges} edge(s) onto {target}; commit {}",
                commit.to_hex()
            );
        }
    }
    Ok(())
}

/// Turn the mutually-exclusive node/edge flags into a [`Mapping`].
fn build_mapping(
    label: Option<&str>,
    edge: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    disc: Option<&str>,
) -> Result<Mapping> {
    match (label, edge) {
        (Some(label), None) => Ok(Mapping::Node {
            label: label.to_owned(),
        }),
        (None, Some(rtype)) => {
            let from = from.context("--edge requires --from LABEL=field[,field...]")?;
            let to = to.context("--edge requires --to LABEL=field[,field...]")?;
            Ok(Mapping::Edge {
                rtype: rtype.to_owned(),
                from: EndpointSpec::parse(from)?,
                to: EndpointSpec::parse(to)?,
                disc: disc.map(str::to_owned),
            })
        }
        (Some(_), Some(_)) => bail!("--label and --edge are mutually exclusive"),
        (None, None) => bail!("import needs either --label (nodes) or --edge (relationships)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_hash_is_stable_and_sensitive() {
        let a = source_hash(b"name,cores\nweb1,8\n");
        let b = source_hash(b"name,cores\nweb1,8\n");
        let c = source_hash(b"name,cores\nweb1,9\n");
        assert_eq!(a, b);
        assert_ne!(a, c);
        // SHA-256 hex is 64 characters.
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn csv_parses_header_and_rows_as_strings() {
        let rows = parse_csv(b"name,cores\nweb1,8\ndb1,16\n").expect("csv");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("name"), Some(&Value::String("web1".into())));
        assert_eq!(rows[0].get("cores"), Some(&Value::String("8".into())));
    }

    #[test]
    fn json_array_and_ndjson_parse_to_typed_values() {
        let json = parse_json(br#"[{"name":"web1","cores":8,"up":true}]"#).expect("json");
        assert_eq!(json[0].get("cores"), Some(&Value::Int(8)));
        assert_eq!(json[0].get("up"), Some(&Value::Bool(true)));

        let nd = parse_ndjson(b"{\"name\":\"web1\"}\n\n{\"name\":\"db1\"}\n").expect("ndjson");
        assert_eq!(nd.len(), 2);
        assert_eq!(nd[1].get("name"), Some(&Value::String("db1".into())));
    }

    #[test]
    fn nested_json_objects_are_rejected() {
        let err = parse_json(br#"[{"meta":{"nested":1}}]"#).unwrap_err();
        assert!(matches!(err, ImportError::Extract(_)));
    }

    #[test]
    fn edge_mapping_consumes_endpoint_fields() {
        let mapping = Mapping::Edge {
            rtype: "PEERS_WITH".into(),
            from: EndpointSpec::parse("Host=src").unwrap(),
            to: EndpointSpec::parse("Host=dst").unwrap(),
            disc: None,
        };
        let mut row = Row::new();
        row.insert("src".into(), Value::String("web1".into()));
        row.insert("dst".into(), Value::String("db1".into()));
        row.insert("weight".into(), Value::String("5".into()));
        let record = map_row(row, &mapping).expect("edge");
        match record {
            ImportRecord::Edge {
                src,
                dst,
                properties,
                ..
            } => {
                assert_eq!(src.key, vec![Value::String("web1".into())]);
                assert_eq!(dst.key, vec![Value::String("db1".into())]);
                // Endpoint fields are consumed; only `weight` remains.
                assert_eq!(properties.len(), 1);
                assert!(properties.contains_key("weight"));
            }
            other => panic!("expected edge, got {other:?}"),
        }
    }

    #[test]
    fn endpoint_spec_parse_validates() {
        assert!(EndpointSpec::parse("Host=name").is_ok());
        assert!(EndpointSpec::parse("Host=a,b").is_ok());
        assert!(EndpointSpec::parse("noequals").is_err());
        assert!(EndpointSpec::parse("=name").is_err());
        assert!(EndpointSpec::parse("Host=").is_err());
    }
}
