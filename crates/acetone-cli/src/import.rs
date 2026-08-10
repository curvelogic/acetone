//! The `import` subcommand: built-in CSV and JSON/NDJSON extractors, source
//! hashing, and the thin wiring to `acetone_core::graph::import` (spec §7,
//! ADR-0021). The orchestration, transform and provenance live in the graph
//! crate; this module only turns a file plus mapping flags into a
//! [`SourceExtractor`] and reports the outcome.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::Path;

use acetone_core::graph::import::{
    EndpointRef, ImportError, ImportOptions, ImportOutcome, ImportRecord, Provenance,
    SourceExtractor,
};
use acetone_core::model::Value;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::output::outln;
use acetone_core::graph::Repository;

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

/// A built-in file extractor: pull rows from the source incrementally,
/// then apply the mapping (ADR-0062). CSV and NDJSON stream in bounded
/// memory; a JSON array is a single value and parses whole (documented
/// residual — use CSV or NDJSON for sources larger than memory).
struct FileExtractor {
    format: Format,
    mapping: Mapping,
    state: ExtractorState,
}

/// The per-record/per-line resident-memory bound (acetone-7qw.4): ADR-0062
/// promises bounded memory under `--batch-size`, which a single pathological
/// record — a 10 GB NDJSON line with no newline, one huge quoted CSV field —
/// previously broke. Generous for any real record; a cap breach is a typed
/// refusal naming the bound.
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

/// A `Read` wrapper enforcing `MAX_RECORD_BYTES` between checkpoints — the
/// csv crate has no record-size limit of its own, but a record cannot span
/// more input bytes than were read since the last yielded record, so the
/// record loop checkpoints after each yield and this wrapper errors when a
/// single record's span exceeds the cap. Bounds allocation without touching
/// the csv crate's quoting logic.
struct CappedRead<R> {
    inner: R,
    since_checkpoint: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<R: Read> Read for CappedRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.since_checkpoint.get() > MAX_RECORD_BYTES {
            return Err(std::io::Error::other(format!(
                "a single record exceeds the {} MiB bound — split the record \
                 or re-shape the source; the bound is not configurable",
                MAX_RECORD_BYTES / (1024 * 1024)
            )));
        }
        let n = self.inner.read(buf)?;
        self.since_checkpoint
            .set(self.since_checkpoint.get() + n as u64);
        Ok(n)
    }
}

/// Read one newline-terminated line, refusing past `MAX_RECORD_BYTES` —
/// `BufReader::lines()` with a bound (acetone-7qw.4). `Ok(None)` is EOF.
fn bounded_line(reader: &mut BufReader<File>) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = reader
        .by_ref()
        .take(MAX_RECORD_BYTES + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    while buf.last() == Some(&b'\n') || buf.last() == Some(&b'\r') {
        buf.pop();
    }
    // Judge the CONTENT length, post-trim — a line of exactly the bound
    // plus its newline is within bounds (PR #253 review minor 1).
    if buf.len() as u64 > MAX_RECORD_BYTES {
        return Err(std::io::Error::other(format!(
            "a single line exceeds the {} MiB bound — split the record or \
             re-shape the source; the bound is not configurable",
            MAX_RECORD_BYTES / (1024 * 1024)
        )));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

enum ExtractorState {
    Csv {
        headers: csv::StringRecord,
        records: csv::StringRecordsIntoIter<BufReader<CappedRead<File>>>,
        record_span: std::rc::Rc<std::cell::Cell<u64>>,
    },
    Ndjson {
        reader: BufReader<File>,
        line_no: usize,
    },
    Json {
        rows: std::vec::IntoIter<Row>,
    },
}

impl FileExtractor {
    /// Build the extractor over an already-open, rewound file handle —
    /// the *same* handle the provenance hash just read, so the hash and
    /// the parse cannot disagree about which file they describe even if
    /// the path is swapped between the two passes (ADR-0062).
    fn from_file(
        format: Format,
        file: File,
        source: &Path,
        mapping: Mapping,
    ) -> Result<FileExtractor> {
        let state = match format {
            Format::Csv => {
                let record_span = std::rc::Rc::new(std::cell::Cell::new(0u64));
                let capped = CappedRead {
                    inner: file,
                    since_checkpoint: std::rc::Rc::clone(&record_span),
                };
                let mut reader = csv::ReaderBuilder::new()
                    .has_headers(true)
                    .from_reader(BufReader::new(capped));
                let headers = reader
                    .headers()
                    .map_err(|e| anyhow::anyhow!("reading CSV header: {e}"))?
                    .clone();
                record_span.set(0);
                ExtractorState::Csv {
                    headers,
                    records: reader.into_records(),
                    record_span,
                }
            }
            Format::Ndjson => ExtractorState::Ndjson {
                reader: BufReader::new(file),
                line_no: 0,
            },
            Format::Json => {
                let mut bytes = Vec::new();
                let mut reader = BufReader::new(file);
                reader
                    .read_to_end(&mut bytes)
                    .with_context(|| format!("reading import source {}", source.display()))?;
                ExtractorState::Json {
                    rows: parse_json(&bytes)
                        .map_err(|e| anyhow::anyhow!("{e}"))?
                        .into_iter(),
                }
            }
        };
        Ok(FileExtractor {
            format,
            mapping,
            state,
        })
    }
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

    fn next_record(&mut self) -> Result<Option<ImportRecord>, ImportError> {
        let row = match &mut self.state {
            ExtractorState::Csv {
                headers,
                records,
                record_span,
            } => match records.next() {
                None => return Ok(None),
                Some(record) => {
                    // Checkpoint the byte-span guard: one record consumed.
                    let record = record
                        .map_err(|e| ImportError::Extract(format!("reading CSV row: {e}")))?;
                    record_span.set(0);
                    let mut row = Row::new();
                    for (name, value) in headers.iter().zip(record.iter()) {
                        row.insert(name.to_owned(), Value::String(value.to_owned()));
                    }
                    row
                }
            },
            ExtractorState::Ndjson { reader, line_no } => loop {
                *line_no += 1;
                let line = bounded_line(reader).map_err(|e| {
                    ImportError::Extract(format!("reading NDJSON line {line_no}: {e}"))
                })?;
                let Some(line) = line else {
                    return Ok(None);
                };
                if line.trim().is_empty() {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(&line).map_err(|e| {
                    ImportError::Extract(format!("parsing NDJSON line {line_no}: {e}"))
                })?;
                break json_object_to_row(&value)?;
            },
            ExtractorState::Json { rows } => match rows.next() {
                None => return Ok(None),
                Some(row) => row,
            },
        };
        map_row(row, &self.mapping).map(Some)
    }
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
/// independent provenance hash (ADR-0021). Streamed in 64 KiB chunks so
/// hashing a source larger than memory stays O(1) resident (ADR-0062);
/// the source is read twice — once to hash, once to parse — which keeps
/// the provenance trailers validated before anything stages.
fn source_hash(file: &mut File, source: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buffer)
            .with_context(|| format!("reading import source {}", source.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    file.seek(std::io::SeekFrom::Start(0))
        .with_context(|| format!("rewinding import source {}", source.display()))?;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

/// Run the `import` subcommand.
#[allow(clippy::too_many_arguments)]
pub fn run(
    repo_path: &Path,
    graph: Option<&str>,
    format: &str,
    source: &Path,
    label: Option<&str>,
    edge: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    disc: Option<&str>,
    branch: Option<&str>,
    message: Option<&str>,
    batch_size: Option<usize>,
) -> Result<()> {
    let repo = crate::commands::open(repo_path, graph)?;
    let outcome = import_core(
        &repo,
        format,
        source,
        &source.display().to_string(),
        label,
        edge,
        from,
        to,
        disc,
        branch,
        message,
        batch_size,
    )?;

    let target = branch.unwrap_or("the current branch");
    match outcome {
        ImportOutcome::NoChange => {
            // The no-op check is graph-level (workspace dirtiness after
            // applying the source), not source-level: the source may never
            // have been imported before and still change nothing, e.g. when
            // it repeats rows the graph already holds (acetone-cbl.3).
            outln!("import produced no graph changes; nothing to commit");
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

/// Import from a source file into `repo`, returning the outcome rather than
/// printing it — the shared core for the CLI `run` (prints) and the daemon's
/// streamed `import` verb (frames the outcome). `source` is the file to read
/// (the CLI's source, or the daemon's private temp file); `provenance_source`
/// is what the commit's provenance records as the origin (the CLI's path, or
/// a "streamed" label for the daemon — the peer never names a path, ADR-0074
/// §4). `repo` is already opened and graph-selected. (acetone-pz0k.4)
#[allow(clippy::too_many_arguments)] // import's CLI flags map 1:1 to args
pub fn import_core(
    repo: &Repository,
    format: &str,
    source: &Path,
    provenance_source: &str,
    label: Option<&str>,
    edge: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    disc: Option<&str>,
    branch: Option<&str>,
    message: Option<&str>,
    batch_size: Option<usize>,
) -> Result<ImportOutcome> {
    let format = Format::parse(format)?;
    let mapping = build_mapping(label, edge, from, to, disc)?;

    let mut file = File::open(source)
        .with_context(|| format!("opening import source {}", source.display()))?;
    let hash = source_hash(&mut file, source)?;
    let mut extractor = FileExtractor::from_file(format, file, source, mapping)?;

    let opts = ImportOptions {
        branch: branch.map(str::to_owned),
        message: message.map(str::to_owned),
        provenance: Provenance {
            source: provenance_source.to_owned(),
            extractor: format.as_str().to_owned(),
            source_hash: hash,
        },
        author: None,
        batch_size,
    };
    acetone_core::graph::import(repo, &mut extractor, opts).context("importing")
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

    fn temp_source(contents: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("source");
        std::fs::write(&path, contents).expect("write");
        (dir, path)
    }

    /// Pull every row through the streaming extractor under a node mapping.
    fn rows_via(format: Format, contents: &[u8]) -> Vec<Row> {
        let (_dir, path) = temp_source(contents);
        let mapping = Mapping::Node { label: "N".into() };
        let file = File::open(&path).expect("open");
        let mut extractor = FileExtractor::from_file(format, file, &path, mapping).expect("open");
        let mut rows = Vec::new();
        while let Some(record) = extractor.next_record().expect("record") {
            match record {
                ImportRecord::Node { properties, .. } => rows.push(properties),
                other => panic!("expected node, got {other:?}"),
            }
        }
        rows
    }

    #[test]
    fn source_hash_is_stable_and_sensitive() {
        let (_d1, p1) = temp_source(b"name,cores\nweb1,8\n");
        let (_d2, p2) = temp_source(b"name,cores\nweb1,8\n");
        let (_d3, p3) = temp_source(b"name,cores\nweb1,9\n");
        let hash_of = |path: &std::path::Path| {
            let mut file = File::open(path).expect("open");
            let hash = source_hash(&mut file, path).expect("hash");
            // The pass leaves the handle rewound for the parse pass.
            assert_eq!(file.stream_position().expect("pos"), 0);
            hash
        };
        let a = hash_of(&p1);
        let b = hash_of(&p2);
        let c = hash_of(&p3);
        assert_eq!(a, b);
        assert_ne!(a, c);
        // SHA-256 hex is 64 characters.
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn csv_streams_header_and_rows_as_strings() {
        let rows = rows_via(Format::Csv, b"name,cores\nweb1,8\ndb1,16\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get("name"), Some(&Value::String("web1".into())));
        assert_eq!(rows[0].get("cores"), Some(&Value::String("8".into())));
        // Quoted fields may contain newlines; the streaming reader must
        // carry them across rows.
        let rows = rows_via(
            Format::Csv,
            b"name,note\nweb1,\"line one\nline two\"\ndb1,plain\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get("note"),
            Some(&Value::String("line one\nline two".into()))
        );
    }

    #[test]
    fn json_array_and_ndjson_parse_to_typed_values() {
        let json = rows_via(Format::Json, br#"[{"name":"web1","cores":8,"up":true}]"#);
        assert_eq!(json[0].get("cores"), Some(&Value::Int(8)));
        assert_eq!(json[0].get("up"), Some(&Value::Bool(true)));

        let nd = rows_via(
            Format::Ndjson,
            b"{\"name\":\"web1\"}\n\n{\"name\":\"db1\"}\n",
        );
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
