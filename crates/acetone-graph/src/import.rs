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
/// `name` is recorded in the `Acetone-Extractor` trailer. Records are
/// *pulled* one at a time (ADR-0062), so a source larger than memory
/// imports in bounded resident memory: the importer stages and saves in
/// batches as it pulls. Contract: a source must yield a node before any
/// edge that references it — referential integrity is enforced at each
/// batch's transaction boundary (ADR-0028), so a forward reference
/// across batches fails the import.
pub trait SourceExtractor {
    /// A stable identifier for this extractor (e.g. `"csv"`), recorded as
    /// provenance.
    fn name(&self) -> &str;
    /// Pull the next canonical record; `Ok(None)` ends the source.
    fn next_record(&mut self) -> Result<Option<ImportRecord>, ImportError>;
}

/// A whole-source extractor over an in-memory record list — the library
/// convenience for callers whose source already fits in memory, and the
/// test harness's workhorse.
pub struct VecExtractor {
    name: String,
    records: std::vec::IntoIter<ImportRecord>,
}

impl VecExtractor {
    /// Wrap `records` as a source named `name` (the extractor trailer).
    pub fn new(name: impl Into<String>, records: Vec<ImportRecord>) -> Self {
        VecExtractor {
            name: name.into(),
            records: records.into_iter(),
        }
    }
}

impl SourceExtractor for VecExtractor {
    fn name(&self) -> &str {
        &self.name
    }
    fn next_record(&mut self) -> Result<Option<ImportRecord>, ImportError> {
        Ok(self.records.next())
    }
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

/// Records staged per transaction batch (ADR-0062). Small enough that a
/// batch's canonical forms are negligible beside the tree caches; large
/// enough that per-save overhead amortises.
pub const DEFAULT_IMPORT_BATCH: usize = 8192;

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
    /// Records staged per transaction batch; `None` means
    /// [`DEFAULT_IMPORT_BATCH`]. The final graph is batch-size independent
    /// (identical roots — Invariant #1); the knob exists for memory
    /// tuning and for the property tests that prove that independence.
    pub batch_size: Option<usize>,
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

    // Records stream in batches (ADR-0062): extraction interleaves with
    // staging, so a failure mid-stream is cleaned up by
    // `import_into_workspace` resetting the workspace to HEAD.
    match &opts.branch {
        None => import_into_workspace(repo, extractor, &opts, &trailers),
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
            let result = import_into_workspace(repo, extractor, &opts, &trailers);
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

/// Stream the source into the workspace in batches, then commit unless
/// the graph is unchanged. The `trailers` are the already-validated
/// provenance trailers from [`run`]. A mid-stream failure after any
/// batch has saved resets the workspace to its committed state, so the
/// caller is never left dirty (and, under `--branch`, never stranded).
fn import_into_workspace(
    repo: &Repository,
    extractor: &mut dyn SourceExtractor,
    opts: &ImportOptions,
    trailers: &[(String, String)],
) -> Result<ImportOutcome, GraphError> {
    let (nodes, edges) = match stream_batches(repo, extractor, opts) {
        Ok(counts) => counts,
        Err(e) => {
            // Best-effort cleanup; the import error always wins over any
            // cleanup failure (an exceptional store failure here leaves
            // the workspace dirty, exactly as a post-save failure always
            // could — recoverable by a later reset).
            let _ = repo.reset_workspace_to_head();
            return Err(e);
        }
    };

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

/// Pull records, canonicalise, constraint-check and stage them in
/// batches of `opts.batch_size` (ADR-0062). Returns `(nodes, edges)`
/// counts of records staged. Referential integrity (dangling edges) is
/// enforced by each batch's transaction on save (ADR-0028), which is
/// where the extractor contract — nodes before the edges that reference
/// them — is enforced.
fn stream_batches(
    repo: &Repository,
    extractor: &mut dyn SourceExtractor,
    opts: &ImportOptions,
) -> Result<(usize, usize), GraphError> {
    let (labels, rtypes) = schema_maps(repo)?;
    let batch_size = opts.batch_size.unwrap_or(DEFAULT_IMPORT_BATCH).max(1);
    let mut tracker = UniqueTracker::seed(repo, &labels)?;
    let mut nodes = 0usize;
    let mut edges = 0usize;
    let mut done = false;
    while !done {
        // Canonicalise one batch — mapping failures surface before this
        // batch stages anything.
        let mut node_puts: Vec<(NodeKey, NodeRecord)> = Vec::new();
        let mut edge_puts: Vec<(EdgeKey, EdgeRecord)> = Vec::new();
        while node_puts.len() + edge_puts.len() < batch_size {
            let Some(record) = extractor.next_record()? else {
                done = true;
                break;
            };
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
        if node_puts.is_empty() && edge_puts.is_empty() {
            continue;
        }

        // Constraints (existence, UNIQUE — spec §2), per batch: the
        // tracker sees the workspace plus every batch so far, with
        // last-record-wins unclaiming, so the checks match the one-shot
        // final-state semantics — except that a transient UNIQUE
        // collision a *later* batch would have resolved is an error
        // under streaming (recorded in ADR-0062).
        let violations = tracker.apply_batch(&labels, &node_puts)?;
        if !violations.is_empty() {
            return Err(
                ImportError::Constraints(crate::constraints::ConstraintViolations(violations))
                    .into(),
            );
        }

        let mut txn = repo.begin_write()?;
        for (key, record) in &node_puts {
            txn.put_node(key, record)?;
        }
        for (key, record) in &edge_puts {
            txn.put_edge(key, record)?;
        }
        txn.save()?;
        nodes += node_puts.len();
        edges += edge_puts.len();
    }
    Ok((nodes, edges))
}

/// Streaming constraint enforcement (ADR-0062). Existence (`REQUIRE`) is
/// per record. UNIQUE claims are tracked per `(label, property,
/// canonical value encoding)` with the claiming keys — seeded by one
/// pass over the workspace at import start (only when some label
/// declares UNIQUE) and maintained across batches with replace-semantics
/// unclaiming. Memory is O(claimed unique values), inherent without a
/// persistent index (index-backed UNIQUE is acetone-ryg); with no UNIQUE
/// constraints declared the tracker holds nothing.
/// A UNIQUE claim's identity: `(label, property, value encoding)`.
type ClaimTriple = (String, String, Vec<u8>);

struct UniqueTracker {
    /// Claim → owner key encodings.
    claims: BTreeMap<ClaimTriple, std::collections::BTreeSet<Vec<u8>>>,
    /// key encoding → the claim triples it currently owns, for unclaiming
    /// when a later record replaces the key.
    by_key: BTreeMap<Vec<u8>, Vec<ClaimTriple>>,
    /// Representative value per claim triple, for violation reporting.
    values: BTreeMap<ClaimTriple, Value>,
    /// Key decode per owner encoding, for violation reporting.
    keys: BTreeMap<Vec<u8>, NodeKey>,
    active: bool,
}

impl UniqueTracker {
    fn seed(
        repo: &Repository,
        labels: &BTreeMap<String, LabelDef>,
    ) -> Result<UniqueTracker, GraphError> {
        let mut tracker = UniqueTracker {
            claims: BTreeMap::new(),
            by_key: BTreeMap::new(),
            values: BTreeMap::new(),
            keys: BTreeMap::new(),
            active: labels.values().any(|def| !def.unique().is_empty()),
        };
        if !tracker.active {
            return Ok(tracker);
        }
        let snapshot = repo.workspace_snapshot()?;
        for (key, record) in snapshot.nodes()? {
            tracker.claim(labels, &key, &record)?;
        }
        Ok(tracker)
    }

    /// Register `(key, record)`'s unique-property claims, unclaiming
    /// whatever the key held before (replace semantics).
    fn claim(
        &mut self,
        labels: &BTreeMap<String, LabelDef>,
        key: &NodeKey,
        record: &NodeRecord,
    ) -> Result<Vec<ClaimTriple>, GraphError> {
        let Some(def) = labels.get(key.label()) else {
            return Ok(Vec::new());
        };
        let mut touched = Vec::new();
        if def.unique().is_empty() {
            return Ok(touched);
        }
        let encoded = key.encode()?;
        if let Some(previous) = self.by_key.remove(&encoded) {
            for triple in previous {
                if let Some(owners) = self.claims.get_mut(&triple) {
                    owners.remove(&encoded);
                    touched.push(triple);
                }
            }
        }
        let mut owned = Vec::new();
        for property in def.unique() {
            if let Some(value) = record.properties().get(property) {
                let value_enc = acetone_model::values::encode_value(value)
                    .map_err(acetone_model::records::RecordEncodeError::from)?;
                let triple = (key.label().to_owned(), property.clone(), value_enc);
                self.claims
                    .entry(triple.clone())
                    .or_default()
                    .insert(encoded.clone());
                self.values.insert(triple.clone(), value.clone());
                touched.push(triple.clone());
                owned.push(triple);
            }
        }
        self.keys.insert(encoded.clone(), key.clone());
        self.by_key.insert(encoded, owned);
        Ok(touched)
    }

    /// Apply one batch's node puts and report the violations they cause:
    /// missing required properties, and UNIQUE claim groups that end the
    /// batch with two or more owners. The last record for a key wins
    /// *before* the checks (`put_node` replace semantics), and checks run
    /// in encoded-key order so violation reports are deterministic —
    /// both matching the former whole-source final-state check. (Across
    /// batches the supersede window has passed: a violating record only
    /// corrected in a later batch errors under streaming — ADR-0062.)
    fn apply_batch(
        &mut self,
        labels: &BTreeMap<String, LabelDef>,
        node_puts: &[(NodeKey, NodeRecord)],
    ) -> Result<Vec<crate::constraints::ConstraintViolation>, GraphError> {
        let mut surviving: BTreeMap<Vec<u8>, &(NodeKey, NodeRecord)> = BTreeMap::new();
        for put in node_puts {
            surviving.insert(put.0.encode()?, put);
        }
        let mut violations = Vec::new();
        let mut touched: std::collections::BTreeSet<ClaimTriple> =
            std::collections::BTreeSet::new();
        for (key, record) in surviving.values() {
            // Existence: a required property must be a key property or
            // present in the record.
            if let Some(def) = labels.get(key.label()) {
                for property in def.exists() {
                    let present = def.key().iter().any(|k| k == property)
                        || record.properties().contains_key(property);
                    if !present {
                        violations.push(crate::constraints::ConstraintViolation::MissingRequired {
                            node: key.clone(),
                            property: property.clone(),
                        });
                    }
                }
            }
            if self.active {
                touched.extend(self.claim(labels, key, record)?);
            }
        }
        for triple in touched {
            let Some(owners) = self.claims.get(&triple) else {
                continue;
            };
            if owners.len() < 2 {
                continue;
            }
            let (label, property, _) = &triple;
            violations.push(crate::constraints::ConstraintViolation::Unique {
                label: label.clone(),
                property: property.clone(),
                value: self.values.get(&triple).cloned().unwrap_or(Value::Null),
                nodes: owners
                    .iter()
                    .filter_map(|encoded| self.keys.get(encoded).cloned())
                    .collect(),
            });
        }
        Ok(violations)
    }
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
