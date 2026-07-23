//! The library-level Cypher query entry point (ADR-0039, `acetone-vf6`).
//!
//! Running a query end-to-end — parse → bind → build the graph/version/procedure
//! adapters over the repository → execute → (for a write) persist and save —
//! used to live only in the CLI. It lives here instead, in the lowest crate that
//! depends on both the executor and `acetone-graph` (the layering forbids a
//! `Repository::query` method: `acetone-graph` must not depend on
//! `acetone-cypher`). `acetone-core` re-exports [`Session`], so a library
//! consumer runs Cypher without re-implementing any of the glue.
//!
//! ```no_run
//! # use acetone_graph::repo::{InitOptions, Repository};
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use acetone_cypher::session::{Outcome, Session};
//!
//! let repo = Repository::open("graph.git".as_ref())?;
//! match Session::new(&repo).run("MATCH (n:Host) RETURN n.name")? {
//!     Outcome::Read(result) => println!("{} rows", result.rows.len()),
//!     Outcome::Write(result) => println!("{} properties set", result.stats.properties_set),
//! }
//! # Ok(()) }
//! ```

use std::collections::BTreeMap;

use acetone_graph::repo::{Repository, Snapshot};
use acetone_model::graph_keys::{EdgeKey, NodeKey};

use crate::ast::{Clause, PathPattern, Query};
use crate::bind::{BindMode, Catalogue, bind};
use crate::exec::value::Value;
use crate::exec::{
    GraphSnapshot, GraphSource, ProcedureProvider, QueryLimits, QueryResult, StoreBackedSource,
    VersionResolver, catalogue_from_schema, execute_versioned_with_limits,
    execute_write_with_limits, key_names_from_schema, virtual_diff_node,
};

/// The result of a [`Session::run`], carrying which kind of query ran so a
/// caller can render a write summary without re-parsing. A `Write` has already
/// advanced the workspace atomically.
#[derive(Debug, Clone)]
pub enum Outcome {
    Read(QueryResult),
    Write(QueryResult),
}

impl Outcome {
    /// The underlying result rows/columns/stats, regardless of read or write.
    pub fn result(&self) -> &QueryResult {
        match self {
            Outcome::Read(r) | Outcome::Write(r) => r,
        }
    }

    /// Whether a write clause ran (and the workspace was advanced).
    pub fn is_write(&self) -> bool {
        matches!(self, Outcome::Write(_))
    }
}

/// An error running a query. Each variant carries the underlying failure; use
/// [`QueryError::render`] with the original query text for a caret diagnostic.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error(transparent)]
    Parse(#[from] crate::ParseError),
    #[error(transparent)]
    Bind(#[from] crate::bind::BindError),
    #[error(transparent)]
    Exec(#[from] crate::exec::ExecError),
    #[error(transparent)]
    Persist(#[from] crate::persist::PersistError),
    #[error(transparent)]
    Graph(#[from] acetone_graph::GraphError),
    #[error("cannot write with a version pin: writes target the workspace, not a past version")]
    WriteAtVersion,
}

impl QueryError {
    /// A user-facing rendering. Parse/bind/execution errors carry a span and
    /// render with the offending line and a caret against `source`; the rest
    /// render as their message.
    pub fn render(&self, source: &str) -> String {
        match self {
            QueryError::Parse(e) => e.render(source),
            QueryError::Bind(e) => e.render(source),
            QueryError::Exec(e) => e.render(source),
            other => other.to_string(),
        }
    }
}

/// A query session over an open [`Repository`]: the library entry point for
/// running openCypher reads and writes, clause-group `AT <version>` and
/// `CALL acetone.*` history procedures (ADR-0039).
pub struct Session<'r> {
    repo: &'r Repository,
}

impl<'r> Session<'r> {
    pub fn new(repo: &'r Repository) -> Self {
        Session { repo }
    }

    /// Run `cypher` against the workspace with default parameters and resource
    /// limits. A read runs against the workspace snapshot; a write runs in a
    /// single-writer transaction and advances the workspace atomically.
    pub fn run(&self, cypher: &str) -> Result<Outcome, QueryError> {
        self.run_with(cypher, &BTreeMap::new(), &QueryLimits::default())
    }

    /// As [`Session::run`], with explicit query parameters and a governor budget.
    pub fn run_with(
        &self,
        cypher: &str,
        parameters: &BTreeMap<String, Value>,
        limits: &QueryLimits,
    ) -> Result<Outcome, QueryError> {
        let parsed = crate::parse(cypher)?;
        if is_write(&parsed) {
            self.run_write(&parsed, cypher, parameters, limits)
                .map(Outcome::Write)
        } else {
            let snapshot = self.repo.workspace_snapshot()?;
            self.run_read(&parsed, cypher, &snapshot, parameters, limits)
                .map(Outcome::Read)
        }
    }

    /// Run a read-only `cypher` against a past version (`refspec`, resolved by
    /// [`Repository::snapshot`]). A write query is rejected — writes target the
    /// live workspace, never a historical version.
    pub fn query_at(&self, cypher: &str, refspec: &str) -> Result<QueryResult, QueryError> {
        let parsed = crate::parse(cypher)?;
        if is_write(&parsed) {
            return Err(QueryError::WriteAtVersion);
        }
        let snapshot = self.repo.snapshot(refspec)?;
        self.run_read(
            &parsed,
            cypher,
            &snapshot,
            &BTreeMap::new(),
            &QueryLimits::default(),
        )
    }

    /// Bind and execute a read against `snapshot`, resolving clause-group
    /// `AT <ref>` and `CALL acetone.*` against the repository.
    ///
    /// The read runs over a lazy [`StoreBackedSource`] (ADR-0040): a
    /// seek/expand-anchored query touches only the matching rows rather than
    /// materialising the whole version. A store read that fails mid-query is
    /// recorded on the source (the [`GraphSource`] trait is infallible) and
    /// drained here into a [`QueryError`] so it surfaces rather than silently
    /// dropping rows.
    fn run_read(
        &self,
        parsed: &Query,
        cypher: &str,
        snapshot: &Snapshot<'_>,
        parameters: &BTreeMap<String, Value>,
        limits: &QueryLimits,
    ) -> Result<QueryResult, QueryError> {
        let schema = snapshot.schema_entries()?;
        let catalogue = catalogue_from_schema(schema.clone());
        let mode = if catalogue.is_empty() {
            BindMode::Lenient
        } else {
            BindMode::Strict
        };
        let bound = bind(cypher, parsed, &catalogue, mode)?;
        let base = StoreBackedSource::new(snapshot, &schema);
        let resolver = StoreResolver {
            repo: self.repo,
            base: &base,
        };
        let procedures = RepoProcedures { repo: self.repo };
        let mut result =
            execute_versioned_with_limits(&bound, &resolver, &procedures, parameters, limits)?;
        // A lazy read error cannot travel through the infallible source trait;
        // surface it now rather than return a silently-incomplete result.
        if let Some(error) = base.take_error() {
            return Err(QueryError::Graph(error));
        }
        result.advisories = undeclared_label_advisories(parsed, &catalogue, &result, &base);
        Ok(result)
    }

    /// Run a write inside a single-writer transaction over the workspace, replay
    /// its net changes and save. The workspace advance is atomic — a failure
    /// leaves it untouched. The caller commits separately.
    fn run_write(
        &self,
        parsed: &Query,
        cypher: &str,
        parameters: &BTreeMap<String, Value>,
        limits: &QueryLimits,
    ) -> Result<QueryResult, QueryError> {
        let mut txn = self.repo.begin_write()?;
        // Read the workspace the transaction locked, and run the query over it.
        let snapshot = self.repo.workspace_snapshot()?;
        let (base, catalogue, mode) = build_base(&snapshot)?;
        let bound = bind(cypher, parsed, &catalogue, mode)?;
        let resolver = RepoResolver {
            repo: self.repo,
            base,
        };
        let (result, changes) = execute_write_with_limits(&bound, &resolver, parameters, limits)?;
        crate::persist::persist_changes(&changes, &mut txn, &catalogue, &snapshot)?;
        txn.save()?;
        Ok(result)
    }
}

/// Whether any clause writes (so the session dispatches to the write path).
fn is_write(parsed: &Query) -> bool {
    parsed.clauses.iter().any(|clause| clause.is_write())
}

/// Advisories for a schema-free read that matched nothing (acetone-7bn.5). In a
/// schema-free repository binding is Lenient, so an undeclared label in a
/// `MATCH` is not an error (openCypher read semantics) — a typo therefore
/// returns 0 rows with no signal, an exploration trap. When a read produced no
/// rows and referenced a label that matches **no node in the graph**, return a
/// non-error note naming the label(s). This never fires with a declared schema
/// (an undeclared label is already a hard error there, acetone-7bn.4) and never
/// changes the result.
///
/// The label is checked against the graph with the executor's own
/// [`GraphSource::nodes_by_labels`], so the note fires only for a genuinely
/// absent label — never for a real, populated label whose `WHERE` filtered the
/// result to empty (which would make "check for a typo" misleading). This check
/// runs only on the narrow schema-free-and-empty-result path.
fn undeclared_label_advisories(
    parsed: &Query,
    catalogue: &Catalogue,
    result: &QueryResult,
    graph: &dyn GraphSource,
) -> Vec<String> {
    if !catalogue.is_empty() || !result.rows.is_empty() {
        return Vec::new();
    }
    let mut labels: Vec<String> = Vec::new();
    for clause in &parsed.clauses {
        if let Clause::Match(m) = clause {
            for path in &m.patterns {
                collect_pattern_labels(path, &mut labels);
            }
        }
    }
    labels.sort();
    labels.dedup();
    // Keep only labels that match no node at all — a genuinely absent (typo'd or
    // never-populated) label, not a real label a `WHERE` filtered to empty.
    labels.retain(|label| {
        graph
            .nodes_by_labels(std::slice::from_ref(label))
            .is_empty()
    });
    if labels.is_empty() {
        return Vec::new();
    }
    let names = labels
        .iter()
        .map(|l| format!("{l:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let plural = if labels.len() == 1 { "label" } else { "labels" };
    vec![format!(
        "note: {plural} {names} not declared and matched no nodes in this schema-free \
         repository — 0 rows. Declare a label with `acetone declare-label <label> \
         --key <property>`, or check for a typo."
    )]
}

/// Collect every label named on the nodes of one path pattern.
fn collect_pattern_labels(path: &PathPattern, out: &mut Vec<String>) {
    out.extend(path.start.labels.iter().cloned());
    for (_, node) in &path.steps {
        out.extend(node.labels.iter().cloned());
    }
}

/// Build the executor's in-memory graph source, the binder catalogue and the
/// bind mode from a stored snapshot. Strict binding when the schema declares
/// structure; a schema-free repository (raw Phase 1 data) stays queryable under
/// openCypher's permissive read semantics (recorded decision `acetone-yzc.6`).
fn build_base(snapshot: &Snapshot<'_>) -> Result<(GraphSnapshot, Catalogue, BindMode), QueryError> {
    let nodes = snapshot.nodes()?;
    let edges = snapshot.edges()?;
    let schema = snapshot.schema_entries()?;
    let base = GraphSnapshot::from_records_with_schema(&nodes, &edges, &schema);
    let catalogue = catalogue_from_schema(schema);
    let mode = if catalogue.is_empty() {
        BindMode::Lenient
    } else {
        BindMode::Strict
    };
    Ok((base, catalogue, mode))
}

/// A version resolver backed by the open repository: clause-group `AT <ref>`
/// reads the graph at that commit. The base version is the snapshot the query
/// runs against (the workspace, or the `AT`/`query_at` version).
struct RepoResolver<'r> {
    repo: &'r Repository,
    base: GraphSnapshot,
}

impl VersionResolver for RepoResolver<'_> {
    fn base(&self) -> &dyn GraphSource {
        &self.base
    }

    fn at(&self, refspec: &str) -> Result<Box<dyn GraphSource>, String> {
        materialise_at(self.repo, refspec)
    }
}

/// A version resolver whose base is the lazy [`StoreBackedSource`] (the read
/// path, ADR-0040). Clause-group `AT <ref>` still materialises a
/// [`GraphSnapshot`]: a boxed `GraphSource` must own its data, so it cannot
/// borrow a per-call snapshot.
struct StoreResolver<'r, 's> {
    repo: &'r Repository,
    base: &'s StoreBackedSource<'s>,
}

impl VersionResolver for StoreResolver<'_, '_> {
    fn base(&self) -> &dyn GraphSource {
        self.base
    }

    fn at(&self, refspec: &str) -> Result<Box<dyn GraphSource>, String> {
        materialise_at(self.repo, refspec)
    }
}

/// Materialise the graph at `refspec` as an owned [`GraphSnapshot`] for a
/// clause-group `AT`. Used by both resolvers (the boxed source must be owned).
fn materialise_at(repo: &Repository, refspec: &str) -> Result<Box<dyn GraphSource>, String> {
    let snapshot = repo.snapshot(refspec).map_err(|e| e.to_string())?;
    let nodes = snapshot.nodes().map_err(|e| e.to_string())?;
    let edges = snapshot.edges().map_err(|e| e.to_string())?;
    let schema = snapshot.schema_entries().map_err(|e| e.to_string())?;
    Ok(Box::new(GraphSnapshot::from_records_with_schema(
        &nodes, &edges, &schema,
    )))
}

/// Serves `CALL acetone.*` history procedures (spec §5.2) from the open
/// repository, so the query executor and the CLI history commands share one
/// implementation (the efficient prolly diff / commit walk). `acetone.diff` and
/// `acetone.log` are backed by `Repository::diff`/`log`; `acetone.blame` and
/// `acetone.conflicts` read the merge state.
struct RepoProcedures<'r> {
    repo: &'r Repository,
}

impl ProcedureProvider for RepoProcedures<'_> {
    fn call(&self, name: &str, args: &[Value]) -> Result<Vec<Vec<Value>>, String> {
        match name {
            "acetone.log" => {
                let refspec = match args.first() {
                    None => None,
                    Some(v) => Some(as_string(v, "acetone.log", "ref")?),
                };
                let entries = self
                    .repo
                    .log(refspec.as_deref())
                    .map_err(|e| e.to_string())?;
                Ok(entries
                    .into_iter()
                    .map(|entry| {
                        let subject = entry.message.lines().next().unwrap_or("").to_string();
                        vec![Value::String(entry.id.to_hex()), Value::String(subject)]
                    })
                    .collect())
            }
            "acetone.diff" => {
                use acetone_graph::diff::ChangeKind;
                let from = as_string(&args[0], "acetone.diff", "from")?;
                let to = as_string(&args[1], "acetone.diff", "to")?;
                let diff = self.repo.diff(&from, &to).map_err(|e| e.to_string())?;
                // The schema of each side names key properties on the virtual
                // nodes: added/modified live in `to`, removed in `from`.
                let from_schema = self
                    .repo
                    .snapshot(&from)
                    .and_then(|s| s.schema_entries())
                    .map_err(|e| e.to_string())?;
                let to_schema = self
                    .repo
                    .snapshot(&to)
                    .and_then(|s| s.schema_entries())
                    .map_err(|e| e.to_string())?;
                // Key-name maps built once per side, not per node (acetone-v8g).
                let from_key_names = key_names_from_schema(&from_schema);
                let to_key_names = key_names_from_schema(&to_schema);
                let mut rows = Vec::new();
                for change in &diff.nodes {
                    let (record, key_names) = match change.kind {
                        ChangeKind::Removed => (change.before.as_ref(), &from_key_names),
                        _ => (change.after.as_ref(), &to_key_names),
                    };
                    let node = match record {
                        Some(rec) => Value::Node(virtual_diff_node(
                            &change.key,
                            rec,
                            key_names,
                            change.kind.label(),
                        )),
                        None => Value::Null,
                    };
                    rows.push(vec![
                        Value::String(change_kind(change.kind).to_string()),
                        Value::String(change.key.label().to_string()),
                        Value::String(acetone_model::display::format_node_key(&change.key)),
                        node,
                    ]);
                }
                for change in &diff.edges {
                    rows.push(vec![
                        Value::String(change_kind(change.kind).to_string()),
                        Value::String(change.key.rtype().to_string()),
                        Value::String(format_edge_key(&change.key)),
                        // Virtual relationships for edge changes are a follow-up.
                        Value::Null,
                    ]);
                }
                Ok(rows)
            }
            "acetone.blame" => {
                let label = as_string(&args[0], "acetone.blame", "label")?;
                // The key is a single-column value: a string (int-or-string
                // heuristic, matching the CLI's put-node/get-node argument
                // parsing) or an integer literal.
                let key_value = match &args[1] {
                    Value::String(s) => parse_scalar(s),
                    Value::Int(n) => acetone_model::Value::Int(*n),
                    other => {
                        return Err(format!(
                            "acetone.blame key must be a string or integer, got {}",
                            other.type_name()
                        ));
                    }
                };
                // Echo the key column from the value actually looked up, not
                // the raw argument: a string the heuristic reinterpreted
                // ('007' -> Int(7)) must not echo a key that was never probed.
                let key_display = match &key_value {
                    acetone_model::Value::Int(n) => n.to_string(),
                    acetone_model::Value::String(s) => s.clone(),
                    other => acetone_model::display::format_value(other),
                };
                let node_key =
                    NodeKey::new(label.as_str(), vec![key_value]).map_err(|e| e.to_string())?;
                let commits = self.repo.blame(&node_key).map_err(|e| e.to_string())?;
                Ok(commits
                    .into_iter()
                    .map(|commit| {
                        vec![
                            Value::String(label.clone()),
                            Value::String(key_display.clone()),
                            Value::String(commit.to_hex()),
                        ]
                    })
                    .collect())
            }
            "acetone.conflicts" => {
                use acetone_graph::conflicts::WorkspaceConflict;
                use acetone_graph::merge::{ConflictMap, Endpoint, GraphViolation};
                // No merge in progress: no conflicts.
                let Some(theirs) = self.repo.merge_head().map_err(|e| e.to_string())? else {
                    return Ok(Vec::new());
                };
                let conflicts = self.repo.conflicts().map_err(|e| e.to_string())?;
                // `ours` is the branch tip during a merge; `theirs` is
                // MERGE_HEAD. The `_Conflict` node shows the ours-side value,
                // falling back to theirs' only when ours deleted the node.
                let ours = self
                    .repo
                    .head_commit()
                    .map_err(|e| e.to_string())?
                    .ok_or("merge in progress but the branch is unborn")?;
                let ours_snap = self
                    .repo
                    .snapshot(&ours.to_hex())
                    .map_err(|e| e.to_string())?;
                let theirs_snap = self
                    .repo
                    .snapshot(&theirs.to_hex())
                    .map_err(|e| e.to_string())?;
                let ours_schema = ours_snap.schema_entries().map_err(|e| e.to_string())?;
                let theirs_schema = theirs_snap.schema_entries().map_err(|e| e.to_string())?;
                // Key-name maps built once per side, not per conflict row
                // (acetone-v8g).
                let ours_key_names = key_names_from_schema(&ours_schema);
                let theirs_key_names = key_names_from_schema(&theirs_schema);
                // The three-way base of the merge (acetone-s7d): a per-property
                // conflict's base/ours/theirs values are re-derived here from the
                // merge base + the two tips, so a user sees all three sides in one
                // call rather than three hand-written `AT` queries. Unrelated
                // histories (no base) surface base values as null.
                let base_snap = match self
                    .repo
                    .merge_base(&ours, &theirs)
                    .map_err(|e| e.to_string())?
                {
                    Some(base) => Some(
                        self.repo
                            .snapshot(&base.to_hex())
                            .map_err(|e| e.to_string())?,
                    ),
                    None => None,
                };
                // A node's value for `property` on one side, as an exec value
                // (null when the node or the property is absent there).
                let node_prop = |snap: Option<&Snapshot<'_>>, key: &NodeKey, property: &str| {
                    let Some(snap) = snap else {
                        return Ok::<Value, String>(Value::Null);
                    };
                    Ok(snap
                        .get_node(key)
                        .map_err(|e| e.to_string())?
                        .as_ref()
                        .and_then(|r| r.properties().get(property))
                        .map(crate::exec::adapter::convert_value)
                        .unwrap_or(Value::Null))
                };

                // Graph-level violations name entities of the *workspace*
                // graph (re-derived live over the resolved merge, ADR-0058 /
                // acetone-jm8), so their `node` column renders from the
                // workspace snapshot, not from either side of the merge.
                let has_graph = conflicts
                    .iter()
                    .any(|c| matches!(c, WorkspaceConflict::Graph(_)));
                let workspace = if has_graph {
                    let snap = self.repo.workspace_snapshot().map_err(|e| e.to_string())?;
                    let schema = snap.schema_entries().map_err(|e| e.to_string())?;
                    let key_names = key_names_from_schema(&schema);
                    Some((snap, key_names))
                } else {
                    None
                };
                // The workspace record for a violating node, as a `_Conflict`
                // virtual node (null when the node is absent).
                let workspace_node = |key: &NodeKey| -> Result<Value, String> {
                    let Some((snap, key_names)) = &workspace else {
                        return Ok(Value::Null);
                    };
                    Ok(match snap.get_node(key).map_err(|e| e.to_string())? {
                        Some(r) => Value::Node(virtual_diff_node(key, &r, key_names, "_Conflict")),
                        None => Value::Null,
                    })
                };

                let mut rows = Vec::new();
                for conflict in conflicts {
                    let (map, key, property) = match conflict {
                        WorkspaceConflict::Cell { map, key, property } => (map, key, property),
                        WorkspaceConflict::Graph(violation) => {
                            // One row per violation — for a UNIQUE collision,
                            // one per colliding node. `kind` names the
                            // violation class; `property` carries the "what,
                            // specifically": the missing/UNIQUE property, or
                            // which endpoint of a dangling relationship is
                            // absent (src/dst).
                            match violation {
                                GraphViolation::DanglingEdge { edge, role, .. } => {
                                    let edge_key =
                                        EdgeKey::decode_fwd(&edge).map_err(|e| e.to_string())?;
                                    rows.push(vec![
                                        Value::String("dangling-edge".into()),
                                        Value::String(edge_key.rtype().to_string()),
                                        Value::String(format_edge_key(&edge_key)),
                                        Value::String(
                                            match role {
                                                Endpoint::Src => "src",
                                                Endpoint::Dst => "dst",
                                            }
                                            .into(),
                                        ),
                                        Value::Null,
                                        Value::Null,
                                        Value::Null,
                                        Value::Null,
                                    ]);
                                }
                                GraphViolation::MissingRequired { node, property } => {
                                    let node_key =
                                        NodeKey::decode(&node).map_err(|e| e.to_string())?;
                                    let node_col = workspace_node(&node_key)?;
                                    rows.push(vec![
                                        Value::String("missing-required".into()),
                                        Value::String(node_key.label().to_string()),
                                        Value::String(acetone_model::display::format_node_key(
                                            &node_key,
                                        )),
                                        Value::String(property),
                                        Value::Null,
                                        Value::Null,
                                        Value::Null,
                                        node_col,
                                    ]);
                                }
                                GraphViolation::UniqueViolation {
                                    label,
                                    property,
                                    nodes,
                                    ..
                                } => {
                                    for node in nodes {
                                        let node_key =
                                            NodeKey::decode(&node).map_err(|e| e.to_string())?;
                                        let node_col = workspace_node(&node_key)?;
                                        rows.push(vec![
                                            Value::String("unique".into()),
                                            Value::String(label.clone()),
                                            Value::String(acetone_model::display::format_node_key(
                                                &node_key,
                                            )),
                                            Value::String(property.clone()),
                                            Value::Null,
                                            Value::Null,
                                            Value::Null,
                                            node_col,
                                        ]);
                                    }
                                }
                            }
                            continue;
                        }
                    };
                    // The conflicted property of a cell-wise merge (ADR-0035),
                    // null for a whole-record conflict.
                    let property_col = match &property {
                        Some(p) => Value::String(p.clone()),
                        None => Value::Null,
                    };
                    let row = match map {
                        ConflictMap::Nodes => {
                            let node_key = NodeKey::decode(&key).map_err(|e| e.to_string())?;
                            let (record, key_names) = match ours_snap
                                .get_node(&node_key)
                                .map_err(|e| e.to_string())?
                            {
                                Some(r) => (Some(r), &ours_key_names),
                                None => (
                                    theirs_snap.get_node(&node_key).map_err(|e| e.to_string())?,
                                    &theirs_key_names,
                                ),
                            };
                            let node = match record {
                                Some(r) => Value::Node(virtual_diff_node(
                                    &node_key,
                                    &r,
                                    key_names,
                                    "_Conflict",
                                )),
                                None => Value::Null,
                            };
                            // The three sides of a per-property conflict; null for
                            // a whole-record conflict (no single property).
                            let (base_v, ours_v, theirs_v) = match &property {
                                Some(p) => (
                                    node_prop(base_snap.as_ref(), &node_key, p)?,
                                    node_prop(Some(&ours_snap), &node_key, p)?,
                                    node_prop(Some(&theirs_snap), &node_key, p)?,
                                ),
                                None => (Value::Null, Value::Null, Value::Null),
                            };
                            vec![
                                Value::String("cell".into()),
                                Value::String(node_key.label().to_string()),
                                Value::String(acetone_model::display::format_node_key(&node_key)),
                                property_col,
                                base_v,
                                ours_v,
                                theirs_v,
                                node,
                            ]
                        }
                        ConflictMap::Edges => {
                            let edge_key = EdgeKey::decode_fwd(&key).map_err(|e| e.to_string())?;
                            // Edge per-property three-way values need a snapshot
                            // edge-record accessor (follow-up); null for now.
                            vec![
                                Value::String("cell".into()),
                                Value::String(edge_key.rtype().to_string()),
                                Value::String(format_edge_key(&edge_key)),
                                property_col,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                            ]
                        }
                        ConflictMap::Schema => vec![
                            Value::String("cell".into()),
                            Value::String("schema".to_string()),
                            Value::String(key.iter().map(|b| format!("{b:02x}")).collect()),
                            property_col,
                            Value::Null,
                            Value::Null,
                            Value::Null,
                            Value::Null,
                        ],
                    };
                    rows.push(row);
                }
                Ok(rows)
            }
            other => Err(format!("unknown procedure {other}")),
        }
    }
}

/// A procedure string argument, or a typed error naming the argument.
fn as_string(value: &Value, procedure: &str, arg: &str) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        other => Err(format!(
            "{procedure} argument {arg} must be a string, got {}",
            other.type_name()
        )),
    }
}

/// The int-or-string heuristic the CLI applies to a single-column key argument:
/// a bare integer parses as [`acetone_model::Value::Int`], anything else stays a
/// string.
fn parse_scalar(raw: &str) -> acetone_model::Value {
    match raw.parse::<i64>() {
        Ok(i) => acetone_model::Value::Int(i),
        Err(_) => acetone_model::Value::String(raw.to_owned()),
    }
}

/// The `kind` yield column for a diff change.
fn change_kind(kind: acetone_graph::diff::ChangeKind) -> &'static str {
    use acetone_graph::diff::ChangeKind;
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
    }
}

/// `src -RTYPE-> dst`, escaped, with a discriminator shown when set (so two
/// parallel edges render distinctly). The canonical renderer lives in the
/// model (`acetone_model::display::format_edge_key`).
fn format_edge_key(key: &EdgeKey) -> String {
    acetone_model::display::format_edge_key(key)
}
