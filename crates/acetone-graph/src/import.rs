//! Import: source rows → canonical node/edge records → bulk upsert → commit
//! with provenance trailers (spec §7, ADR-0021).
//!
//! This module owns the *plugin interface* and the schema-driven transform
//! and orchestration; it depends only on `acetone-model` schema types, so it
//! carries no format-parsing dependencies. The built-in CSV and JSON/NDJSON
//! extractors live in the thin CLI, where file I/O belongs.
//!
//! An [`SourceExtractor`] yields schema-agnostic [`ImportRecord`]s — labelled
//! property bags carrying *all* fields, key and non-key alike. [`run`] then
//! uses the target label's declared key tuple to split key properties out and
//! build the canonical `(NodeKey, NodeRecord)` (mirroring the Cypher write
//! path, and preserving Invariant #3: key properties never appear in a
//! `NodeRecord`). Records are applied with `put_node`/`put_edge`, which
//! *replace* the record for a key — import is **authoritative**: the source is
//! the source of truth for the records it carries. That is exactly the
//! semantic under which "unchanged source ⇒ no-op" holds.

use std::collections::BTreeMap;

use acetone_model::Value;
use acetone_model::graph_keys::{EdgeKey, NodeKey};
use acetone_model::records::{EdgeRecord, NodeRecord};
use acetone_model::schema::{LabelDef, PropertyType, RelTypeDef, SchemaEntry};
use acetone_store::{Hash, Signature};

use crate::error::GraphError;
use crate::repo::Repository;

/// Extractor- and mapping-side failures. Kept coarse (two message-carrying
/// variants) so the trait is self-contained in `acetone-graph`; the built-in
/// CLI extractors produce [`ImportError::Extract`] with format-specific text.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The extractor could not read or parse its source.
    #[error("import source: {0}")]
    Extract(String),
    /// A source record could not be mapped to a canonical record (missing
    /// key property, unknown label, un-coercible value, …).
    #[error("import mapping: {0}")]
    Mapping(String),
    /// The import was invoked in a way that cannot proceed (e.g. `--branch`
    /// naming the current branch).
    #[error("import: {0}")]
    Config(String),
    /// The imported data violates declared schema constraints (existence or
    /// UNIQUE, spec §2). The whole import is rejected before anything is
    /// staged, so the workspace is untouched (acetone-9gw).
    #[error("import violates declared constraints — {0}")]
    Constraints(crate::constraints::ConstraintViolations),
}

/// One canonical record produced by an extractor. Nodes and edges carry *all*
/// their source fields as properties; the schema-driven transform in [`run`]
/// separates key properties from the record.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportRecord {
    /// A node of `label` whose `properties` include its key properties.
    Node {
        /// The node's primary label.
        label: String,
        /// All source fields, key and non-key alike.
        properties: BTreeMap<String, Value>,
    },
    /// A relationship of `rtype` between two endpoints.
    Edge {
        /// The relationship type.
        rtype: String,
        /// The source endpoint.
        src: EndpointRef,
        /// The destination endpoint.
        dst: EndpointRef,
        /// The discriminator (`Value::Null` for the default; parallel edges
        /// need a declared discriminator, spec §2).
        discriminator: Value,
        /// Edge properties.
        properties: BTreeMap<String, Value>,
    },
}

/// A reference to an edge endpoint by label and key values (in the label's
/// declared key order).
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointRef {
    /// The endpoint node's primary label.
    pub label: String,
    /// The endpoint node's key values, in declared key order.
    pub key: Vec<Value>,
}

/// A source extractor: a deterministic map from a source to canonical records.
///
/// `name` is recorded in the `Acetone-Extractor` trailer. `extract` reads the
/// whole source; imports are bounded in v0.1 (a batched, streaming extractor
/// interface can arrive later without changing the transform).
pub trait SourceExtractor {
    /// A stable identifier for this extractor (e.g. `"csv"`), recorded as
    /// provenance.
    fn name(&self) -> &str;
    /// Produce the canonical records for the whole source.
    fn extract(&mut self) -> Result<Vec<ImportRecord>, ImportError>;
}

/// Provenance recorded in commit trailers (spec §3.5).
#[derive(Debug, Clone)]
pub struct Provenance {
    /// A description of the source (e.g. a file path). → `Acetone-Source`.
    pub source: String,
    /// The extractor identifier. → `Acetone-Extractor`.
    pub extractor: String,
    /// A hash of the raw source bytes (hex). → `Acetone-Source-Hash`.
    pub source_hash: String,
}

/// Options for one import run.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Import onto this branch in isolation, leaving the caller's branch
    /// unchanged; created if absent, checked out (and appended to) if present.
    pub branch: Option<String>,
    /// Commit message (a default is synthesised from the provenance and
    /// counts when `None`).
    pub message: Option<String>,
    /// Provenance for the commit trailers.
    pub provenance: Provenance,
    /// Commit author (defaults to the neutral acetone signature when `None`).
    pub author: Option<Signature>,
}

/// The result of an import run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    /// The source produced no change to the graph — no commit was written.
    NoChange,
    /// A commit was written.
    Committed {
        /// The new commit's address.
        commit: Hash,
        /// Nodes upserted.
        nodes: usize,
        /// Edges upserted.
        edges: usize,
    },
}

/// Import from `extractor` into `repo` per `opts` (spec §7, ADR-0021).
///
/// Requires a clean workspace. With `opts.branch` set, the import lands on that
/// branch and the caller's original branch is checked back out afterwards.
/// Detects no-ops via [`Repository::is_dirty`] and writes no commit for them.
pub fn run(
    repo: &Repository,
    extractor: &mut dyn SourceExtractor,
    opts: ImportOptions,
) -> Result<ImportOutcome, GraphError> {
    // A dirty workspace would fold pre-existing staged edits into the import
    // commit, muddying provenance and no-op detection. Refuse up front.
    if repo.is_dirty()? {
        return Err(GraphError::DirtyWorkspace);
    }

    // Validate the provenance trailer values *before* staging anything. The
    // source string is user-controlled (a file path); an unsuitable value
    // (control character, leading/trailing whitespace) is otherwise only
    // rejected inside `commit()`, i.e. after `save()` has already advanced the
    // workspace — which would leave it dirty and, under `--branch`, strand the
    // caller on the side branch. Failing here keeps the workspace pristine.
    let trailers = provenance_trailers(&opts.provenance);
    for (token, value) in &trailers {
        acetone_store::validate_trailer(token, value)?;
    }

    // Extract before touching the workspace: a parse failure leaves the
    // repository untouched.
    let records = extractor.extract()?;

    match &opts.branch {
        None => import_into_workspace(repo, records, &opts, &trailers),
        Some(branch) => {
            let original = repo.current_branch()?.ok_or(GraphError::NoCurrentBranch)?;
            let original = repo
                .namespace()
                .branch_name(&original)
                .unwrap_or(&original)
                .to_owned();
            if branch == &original {
                return Err(ImportError::Config(format!(
                    "--branch {branch:?} is the current branch; import onto a \
                     different branch for isolation"
                ))
                .into());
            }
            switch_to_branch(repo, branch)?;
            let result = import_into_workspace(repo, records, &opts, &trailers);
            // Return to the original branch. Provenance trailers were validated
            // up front, so the realistic post-save failure is gone and the
            // workspace is clean in every ordinary terminal state (no-op ⇒
            // matches HEAD; committed ⇒ matches the new HEAD; error before
            // save ⇒ untouched); the checkout back then succeeds. A residual
            // *exceptional* store failure after save could still leave the
            // workspace advanced, in which case the restore's own error is
            // surfaced rather than swallowed.
            let restored = repo.checkout_branch(&original);
            match (result, restored) {
                // Import error takes precedence over any restore error.
                (Err(e), _) => Err(e),
                // Import succeeded but we could not get back — surface that.
                (Ok(_), Err(e)) => Err(e),
                (Ok(outcome), Ok(())) => Ok(outcome),
            }
        }
    }
}

/// The three provenance trailers, in a stable order.
fn provenance_trailers(provenance: &Provenance) -> Vec<(String, String)> {
    vec![
        ("Acetone-Source".to_owned(), provenance.source.clone()),
        ("Acetone-Extractor".to_owned(), provenance.extractor.clone()),
        (
            "Acetone-Source-Hash".to_owned(),
            provenance.source_hash.clone(),
        ),
    ]
}

/// Create `branch` (or check it out if it exists) and switch to it.
fn switch_to_branch(repo: &Repository, branch: &str) -> Result<(), GraphError> {
    match repo.create_branch(branch, None) {
        Ok(_) => {}
        Err(GraphError::BranchExists { .. }) => {}
        Err(e) => return Err(e),
    }
    repo.checkout_branch(branch)
}

/// Map every record to canonical form, validate declared constraints, then
/// stage, save and commit unless the graph is unchanged. The `trailers` are
/// the already-validated provenance trailers from [`run`].
fn import_into_workspace(
    repo: &Repository,
    records: Vec<ImportRecord>,
    opts: &ImportOptions,
    trailers: &[(String, String)],
) -> Result<ImportOutcome, GraphError> {
    let (labels, rtypes) = schema_maps(repo)?;

    // Phase 1: map every record to its canonical form — nothing staged yet,
    // so any failure leaves the workspace untouched.
    let mut node_puts: Vec<(NodeKey, NodeRecord)> = Vec::new();
    let mut edge_puts: Vec<(EdgeKey, EdgeRecord)> = Vec::new();
    for record in records {
        match record {
            ImportRecord::Node { label, properties } => {
                let def = labels.get(&label).ok_or_else(|| {
                    ImportError::Mapping(format!(
                        "no schema for label {label:?}; declare it before importing"
                    ))
                })?;
                node_puts.push(node_key_and_record(&label, def, properties)?);
            }
            ImportRecord::Edge {
                rtype,
                src,
                dst,
                discriminator,
                properties,
            } => {
                let src_key = endpoint_key(&src, &labels)?;
                let dst_key = endpoint_key(&dst, &labels)?;
                let props = match rtypes.get(&rtype) {
                    Some(def) => coerce_props(properties, def.types())?,
                    None => properties,
                };
                let edge = EdgeKey::new(src_key, rtype, dst_key, discriminator)?;
                edge_puts.push((edge, EdgeRecord::new(props)));
            }
        }
    }
    let nodes = node_puts.len();
    let edges = edge_puts.len();

    // Phase 2: enforce declared constraints (existence, UNIQUE — spec §2)
    // over the would-be final state, exactly as the Cypher write path would
    // have (acetone-9gw). The final state is the current workspace overlaid
    // with the imported records, last record per key winning (mirroring
    // `put_node`'s replace semantics). Only violations involving an imported
    // key fail the import: a pre-existing breach the import does not touch
    // is fsck's business, not this source's.
    {
        let snapshot = repo.workspace_snapshot()?;
        let mut final_nodes = crate::constraints::NodeSet::new();
        for (key, record) in snapshot.nodes()? {
            final_nodes.insert(key.encode()?, (key, record));
        }
        let mut imported: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
        for (key, record) in &node_puts {
            let encoded = key.encode()?;
            imported.insert(encoded.clone());
            final_nodes.insert(encoded, (key.clone(), record.clone()));
        }
        let violations = crate::constraints::check_nodes(&labels, &final_nodes, Some(&imported))?;
        if !violations.is_empty() {
            return Err(
                ImportError::Constraints(crate::constraints::ConstraintViolations(violations))
                    .into(),
            );
        }
    }

    // Phase 3: stage and save. Referential integrity (dangling edges) is
    // enforced by the transaction itself on save.
    let mut txn = repo.begin_write()?;
    for (key, record) in &node_puts {
        txn.put_node(key, record)?;
    }
    for (key, record) in &edge_puts {
        txn.put_edge(key, record)?;
    }
    txn.save()?;

    if !repo.is_dirty()? {
        return Ok(ImportOutcome::NoChange);
    }

    let message = opts
        .message
        .clone()
        .unwrap_or_else(|| default_message(&opts.provenance, nodes, edges));

    let txn = repo.begin_write()?;
    let commit = txn.commit(&message, trailers, opts.author.clone())?;
    Ok(ImportOutcome::Committed {
        commit,
        nodes,
        edges,
    })
}

/// The label and relationship-type definitions of the current workspace,
/// indexed by name.
type SchemaMaps = (BTreeMap<String, LabelDef>, BTreeMap<String, RelTypeDef>);

/// Read the current workspace's label and relationship-type definitions.
fn schema_maps(repo: &Repository) -> Result<SchemaMaps, GraphError> {
    let snapshot = repo.workspace_snapshot()?;
    let mut labels = BTreeMap::new();
    let mut rtypes = BTreeMap::new();
    for entry in snapshot.schema_entries()? {
        match entry {
            SchemaEntry::Label { name, def } => {
                labels.insert(name, def);
            }
            SchemaEntry::RelType { name, def } => {
                rtypes.insert(name, def);
            }
            SchemaEntry::Index { .. } => {}
        }
    }
    Ok((labels, rtypes))
}

/// Split a node's property bag into `(NodeKey, NodeRecord)` using the label's
/// declared key tuple, coercing each property to its declared type. Key
/// properties are excluded from the record (Invariant #3).
fn node_key_and_record(
    label: &str,
    def: &LabelDef,
    properties: BTreeMap<String, Value>,
) -> Result<(NodeKey, NodeRecord), GraphError> {
    let properties = coerce_props(properties, def.types())?;

    let key_names = def.key();
    let mut key_values = Vec::with_capacity(key_names.len());
    for name in key_names {
        let value = properties.get(name).cloned().ok_or_else(|| {
            ImportError::Mapping(format!(
                "record for {label:?} is missing key property {name:?}"
            ))
        })?;
        key_values.push(value);
    }
    // `NodeKey::new` rejects null/NaN/non-scalar keys (Invariant #3).
    let node_key = NodeKey::new(label.to_owned(), key_values)?;

    let record_props = properties
        .into_iter()
        .filter(|(name, _)| !key_names.iter().any(|k| k == name))
        .collect();
    // Import sets a single primary label; no secondary labels.
    Ok((
        node_key,
        NodeRecord::new(std::iter::empty::<String>(), record_props),
    ))
}

/// Build an endpoint's `NodeKey`, coercing its key values to the endpoint
/// label's declared key-property types.
fn endpoint_key(
    endpoint: &EndpointRef,
    labels: &BTreeMap<String, LabelDef>,
) -> Result<NodeKey, GraphError> {
    let def = labels.get(&endpoint.label).ok_or_else(|| {
        ImportError::Mapping(format!(
            "no schema for endpoint label {:?}; declare it before importing",
            endpoint.label
        ))
    })?;
    let key_names = def.key();
    if endpoint.key.len() != key_names.len() {
        return Err(ImportError::Mapping(format!(
            "endpoint {:?} has {} key value(s) but its key tuple has {}",
            endpoint.label,
            endpoint.key.len(),
            key_names.len()
        ))
        .into());
    }
    let mut values = Vec::with_capacity(key_names.len());
    for (name, value) in key_names.iter().zip(endpoint.key.iter()) {
        values.push(coerce(value.clone(), def.types().get(name).copied())?);
    }
    Ok(NodeKey::new(endpoint.label.clone(), values)?)
}

/// Coerce every property that has a declared type; pass the rest through.
fn coerce_props(
    properties: BTreeMap<String, Value>,
    types: &BTreeMap<String, PropertyType>,
) -> Result<BTreeMap<String, Value>, GraphError> {
    let mut out = BTreeMap::new();
    for (name, value) in properties {
        let coerced = coerce(value, types.get(&name).copied())?;
        out.insert(name, coerced);
    }
    Ok(out)
}

/// Coerce one value to a declared property type. `None` (no declared type)
/// passes the value through unchanged. Coercion is total and deterministic:
/// strings are parsed for scalar targets; a value already of the target type
/// is kept; anything else is a mapping error. Temporal/bytes/list targets
/// accept only an already-correct value in v0.1 (source parsing of those is
/// deferred).
fn coerce(value: Value, ptype: Option<PropertyType>) -> Result<Value, GraphError> {
    let Some(ptype) = ptype else {
        return Ok(value);
    };
    let coerced = match (ptype, value) {
        // Null passes through for any type: existence/key checks catch a null
        // where one is disallowed, with a clearer message than coercion would.
        (_, Value::Null) => Value::Null,

        (PropertyType::String, Value::String(s)) => Value::String(s),

        (PropertyType::Int, Value::Int(i)) => Value::Int(i),
        (PropertyType::Int, Value::String(s)) => {
            Value::Int(parse_scalar(&s, "int", |s| s.trim().parse::<i64>().ok())?)
        }

        (PropertyType::Float, Value::Float(f)) => Value::Float(f),
        (PropertyType::Float, Value::Int(i)) => Value::Float(i as f64),
        (PropertyType::Float, Value::String(s)) => {
            Value::Float(parse_scalar(&s, "float", |s| s.trim().parse::<f64>().ok())?)
        }

        (PropertyType::Bool, Value::Bool(b)) => Value::Bool(b),
        (PropertyType::Bool, Value::String(s)) => {
            Value::Bool(parse_scalar(&s, "bool", |s| match s.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            })?)
        }

        (PropertyType::Bytes, v @ Value::Bytes(_)) => v,
        (PropertyType::Date, v @ Value::Date(_)) => v,
        (PropertyType::Time, v @ Value::Time(_)) => v,
        (PropertyType::DateTime, v @ Value::DateTime(_)) => v,
        (PropertyType::Duration, v @ Value::Duration(_)) => v,
        (PropertyType::List, v @ Value::List(_)) => v,

        (ptype, value) => {
            return Err(ImportError::Mapping(format!(
                "cannot coerce {} to {}",
                value_kind(&value),
                ptype.as_str()
            ))
            .into());
        }
    };
    Ok(coerced)
}

/// Parse a scalar from a string, mapping failure to a mapping error.
fn parse_scalar<T>(s: &str, ty: &str, parse: impl Fn(&str) -> Option<T>) -> Result<T, GraphError> {
    parse(s).ok_or_else(|| ImportError::Mapping(format!("{s:?} is not a valid {ty}")).into())
}

/// A human-readable kind name for an actual value (for error messages).
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::String(_) => "string",
        Value::Bytes(_) => "bytes",
        Value::Date(_) => "date",
        Value::Time(_) => "time",
        Value::DateTime(_) => "datetime",
        Value::Duration(_) => "duration",
        Value::List(_) => "list",
    }
}

/// A default commit message when the caller supplies none.
fn default_message(provenance: &Provenance, nodes: usize, edges: usize) -> String {
    format!(
        "Import {} node(s) and {} edge(s) from {} via {}",
        nodes, edges, provenance.source, provenance.extractor
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coerce_ok(value: Value, ty: PropertyType) -> Value {
        coerce(value, Some(ty)).expect("coercion should succeed")
    }

    fn coerce_err(value: Value, ty: PropertyType) {
        assert!(
            coerce(value, Some(ty)).is_err(),
            "coercion should have failed"
        );
    }

    #[test]
    fn no_declared_type_passes_through() {
        assert_eq!(
            coerce(Value::String("x".into()), None).unwrap(),
            Value::String("x".into())
        );
    }

    #[test]
    fn strings_parse_to_scalar_targets() {
        assert_eq!(
            coerce_ok(Value::String("42".into()), PropertyType::Int),
            Value::Int(42)
        );
        assert_eq!(
            coerce_ok(Value::String("  -7 ".into()), PropertyType::Int),
            Value::Int(-7)
        );
        assert_eq!(
            coerce_ok(Value::String("3.5".into()), PropertyType::Float),
            Value::Float(3.5)
        );
        assert_eq!(
            coerce_ok(Value::String("true".into()), PropertyType::Bool),
            Value::Bool(true)
        );
        assert_eq!(
            coerce_ok(Value::String("false".into()), PropertyType::Bool),
            Value::Bool(false)
        );
    }

    #[test]
    fn already_typed_values_are_kept() {
        assert_eq!(coerce_ok(Value::Int(9), PropertyType::Int), Value::Int(9));
        assert_eq!(
            coerce_ok(Value::String("s".into()), PropertyType::String),
            Value::String("s".into())
        );
    }

    #[test]
    fn int_widens_to_float_but_not_the_reverse() {
        assert_eq!(
            coerce_ok(Value::Int(4), PropertyType::Float),
            Value::Float(4.0)
        );
        // A float is not silently narrowed to an int.
        coerce_err(Value::Float(4.0), PropertyType::Int);
    }

    #[test]
    fn unparseable_scalars_and_type_mismatches_error() {
        coerce_err(Value::String("notanint".into()), PropertyType::Int);
        coerce_err(Value::String("maybe".into()), PropertyType::Bool);
        // A number where a string is declared is a mismatch, not a stringify.
        coerce_err(Value::Int(3), PropertyType::String);
        // Temporal/bytes targets accept only an already-correct value in v0.1.
        coerce_err(Value::String("2020-01-01".into()), PropertyType::Date);
    }

    #[test]
    fn null_passes_through_for_any_declared_type() {
        // Null is left for the key/existence checks to reject with a clearer
        // message than coercion would give.
        assert_eq!(coerce_ok(Value::Null, PropertyType::Int), Value::Null);
        assert_eq!(coerce_ok(Value::Null, PropertyType::String), Value::Null);
    }
}
