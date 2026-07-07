//! The `query` command and `shell` REPL (spec §5, §7): parse → bind →
//! execute an openCypher read query against a stored graph version, and
//! render the result.

use std::collections::BTreeMap;
use std::io::{self, Write};

use acetone_cypher::bind::BindMode;
use acetone_cypher::exec::value::{NodeValue, RelValue, Value};
use acetone_cypher::exec::{GraphSnapshot, QueryResult, catalogue_from_schema};
use acetone_graph::Repository;
use anyhow::{Context, Result, anyhow};

use crate::output::outln;
use crate::value::sanitise_line;

/// Output format for query results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Json,
    Csv,
}

impl Format {
    pub fn parse(name: &str) -> Result<Format> {
        match name {
            "table" => Ok(Format::Table),
            "json" => Ok(Format::Json),
            "csv" => Ok(Format::Csv),
            other => Err(anyhow!("unknown format {other:?} (table, json or csv)")),
        }
    }
}

/// Run one query and print the result.
pub fn run(
    repo_path: &std::path::Path,
    cypher: &str,
    at: Option<&str>,
    format: Format,
) -> Result<()> {
    let repo = Repository::open(repo_path).context("opening repository")?;
    // A write query mutates the workspace inside a transaction; a read query
    // runs against an immutable snapshot.
    let parsed = acetone_cypher::parse(cypher).map_err(|e| anyhow!("{}", e.render(cypher)))?;
    if parsed.clauses.iter().any(|clause| clause.is_write()) {
        if at.is_some() {
            return Err(anyhow!(
                "cannot write with --at: writes target the workspace, not a past version"
            ));
        }
        return run_write(&repo, cypher, format);
    }
    let snapshot = match at {
        Some(refspec) => repo
            .snapshot(refspec)
            .with_context(|| format!("reading at {refspec:?}"))?,
        None => repo.workspace_snapshot().context("reading the workspace")?,
    };
    let result = execute_query(&repo, &snapshot, cypher)?;
    render(&result, format);
    Ok(())
}

/// Execute a write query: run it inside a single-writer transaction over the
/// workspace, replay its net changes into the workspace and save (the user
/// commits separately with `acetone commit`). The workspace advance is
/// atomic — a failure leaves it untouched.
fn run_write(repo: &Repository, cypher: &str, format: Format) -> Result<()> {
    let mut txn = repo.begin_write().context("starting a write transaction")?;
    // Read the workspace the transaction locked, and run the query over it.
    let snapshot = repo.workspace_snapshot().context("reading the workspace")?;
    let nodes = snapshot.nodes().context("reading nodes")?;
    let edges = snapshot.edges().context("reading edges")?;
    let schema = snapshot.schema_entries().context("reading schema")?;

    let base = GraphSnapshot::from_records_with_schema(&nodes, &edges, &schema);
    let catalogue = catalogue_from_schema(schema);
    let mode = if catalogue.is_empty() {
        BindMode::Lenient
    } else {
        BindMode::Strict
    };
    let parsed = acetone_cypher::parse(cypher).map_err(|e| anyhow!("{}", e.render(cypher)))?;
    let bound = acetone_cypher::bind::bind(cypher, &parsed, &catalogue, mode)
        .map_err(|e| anyhow!("{}", e.render(cypher)))?;
    let resolver = RepoResolver { repo, base };
    let (result, changes) =
        acetone_cypher::exec::execute_write(&bound, &resolver, &BTreeMap::new())
            .map_err(|e| anyhow!("{e}"))?;

    acetone_cypher::persist::persist_changes(&changes, &mut txn, &catalogue, &snapshot)
        .map_err(|e| anyhow!("{e}"))?;
    txn.save().context("saving the workspace")?;

    render(&result, format);
    render_write_summary(&result.stats);
    Ok(())
}

/// A one-line summary of a write's side effects (openCypher counts).
fn render_write_summary(stats: &acetone_cypher::exec::WriteSummary) {
    if stats.is_empty() {
        outln!("(no changes)");
        return;
    }
    let mut parts = Vec::new();
    let mut add = |n: u64, singular: &str, plural: &str| {
        if n > 0 {
            parts.push(format!("{n} {}", if n == 1 { singular } else { plural }));
        }
    };
    add(stats.nodes_created, "node created", "nodes created");
    add(
        stats.relationships_created,
        "relationship created",
        "relationships created",
    );
    add(stats.properties_set, "property set", "properties set");
    add(stats.labels_added, "label added", "labels added");
    add(stats.labels_removed, "label removed", "labels removed");
    add(stats.nodes_deleted, "node deleted", "nodes deleted");
    add(
        stats.relationships_deleted,
        "relationship deleted",
        "relationships deleted",
    );
    outln!("{}", parts.join(", "));
}

/// A version resolver backed by the open repository: clause-group
/// `AT <ref>` reads the graph at that commit. The base version is the
/// snapshot the query is run against (workspace, or the `--at` version).
struct RepoResolver<'r> {
    repo: &'r Repository,
    base: GraphSnapshot,
}

impl acetone_cypher::exec::VersionResolver for RepoResolver<'_> {
    fn base(&self) -> &dyn acetone_cypher::exec::GraphSource {
        &self.base
    }

    fn at(&self, refspec: &str) -> Result<Box<dyn acetone_cypher::exec::GraphSource>, String> {
        let snapshot = self.repo.snapshot(refspec).map_err(|e| e.to_string())?;
        let nodes = snapshot.nodes().map_err(|e| e.to_string())?;
        let edges = snapshot.edges().map_err(|e| e.to_string())?;
        let schema = snapshot.schema_entries().map_err(|e| e.to_string())?;
        Ok(Box::new(GraphSnapshot::from_records_with_schema(
            &nodes, &edges, &schema,
        )))
    }
}

/// Serves `CALL acetone.*` history procedures (spec §5.2) from the open
/// repository, so the query executor and the CLI history commands share one
/// implementation (the efficient prolly diff / commit walk). `acetone.diff`
/// and `acetone.log` are backed by `Repository::diff`/`log`; `acetone.blame`
/// and `acetone.conflicts` await their data (acetone-14c.6 / acetone-14c.4).
struct RepoProcedures<'r> {
    repo: &'r Repository,
}

impl acetone_cypher::exec::ProcedureProvider for RepoProcedures<'_> {
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
                let mut rows = Vec::new();
                for change in &diff.nodes {
                    let (record, schema) = match change.kind {
                        ChangeKind::Removed => (change.before.as_ref(), from_schema.as_slice()),
                        _ => (change.after.as_ref(), to_schema.as_slice()),
                    };
                    // The `node` column: the changed node as a virtual value
                    // labelled with its change kind (acetone-14c.1).
                    let node = match record {
                        Some(rec) => Value::Node(acetone_cypher::exec::virtual_diff_node(
                            &change.key,
                            rec,
                            schema,
                            change.kind.label(),
                        )),
                        None => Value::Null,
                    };
                    rows.push(vec![
                        Value::String(change_kind(change.kind).to_string()),
                        Value::String(change.key.label().to_string()),
                        Value::String(crate::commands::format_node_key(&change.key)),
                        node,
                    ]);
                }
                for change in &diff.edges {
                    rows.push(vec![
                        Value::String(change_kind(change.kind).to_string()),
                        Value::String(change.key.rtype().to_string()),
                        Value::String(crate::commands::format_edge_key(&change.key)),
                        // Virtual relationships for edge changes are a follow-up.
                        Value::Null,
                    ]);
                }
                Ok(rows)
            }
            "acetone.blame" => {
                use acetone_model::graph_keys::NodeKey;
                let label = as_string(&args[0], "acetone.blame", "label")?;
                // The key is a single-column value (like put-node/get-node): a
                // string (int-or-string heuristic) or an integer literal.
                let (key_value, key_display) = match &args[1] {
                    Value::String(s) => (crate::value::parse_value(s), s.clone()),
                    Value::Int(n) => (acetone_model::Value::Int(*n), n.to_string()),
                    other => {
                        return Err(format!(
                            "acetone.blame key must be a string or integer, got {}",
                            other.type_name()
                        ));
                    }
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
                use acetone_graph::conflicts::PersistedConflict;
                use acetone_graph::merge::ConflictMap;
                use acetone_model::graph_keys::{EdgeKey, NodeKey};
                // No merge in progress: no conflicts.
                let Some(theirs) = self.repo.merge_head().map_err(|e| e.to_string())? else {
                    return Ok(Vec::new());
                };
                let conflicts = self.repo.conflicts().map_err(|e| e.to_string())?;
                // `ours` is the branch tip during a merge; `theirs` is
                // MERGE_HEAD. The `_Conflict` node shows the **ours-side**
                // value (the current branch's), falling back to theirs' only
                // when ours deleted the node. Base/ours/theirs side-by-side is
                // a later refinement; `CALL acetone.diff` shows the full
                // three-way detail.
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

                let mut rows = Vec::new();
                for conflict in conflicts {
                    let PersistedConflict::Cell { map, key } = conflict else {
                        // Graph violations are not persisted (acetone-14c.4a).
                        continue;
                    };
                    let row = match map {
                        ConflictMap::Nodes => {
                            let node_key = NodeKey::decode(&key).map_err(|e| e.to_string())?;
                            // The conflicting node as a virtual `_Conflict` node.
                            let (record, schema) = match ours_snap
                                .get_node(&node_key)
                                .map_err(|e| e.to_string())?
                            {
                                Some(r) => (Some(r), &ours_schema),
                                None => (
                                    theirs_snap.get_node(&node_key).map_err(|e| e.to_string())?,
                                    &theirs_schema,
                                ),
                            };
                            let node = match record {
                                Some(r) => Value::Node(acetone_cypher::exec::virtual_diff_node(
                                    &node_key,
                                    &r,
                                    schema,
                                    "_Conflict",
                                )),
                                None => Value::Null,
                            };
                            vec![
                                Value::String(node_key.label().to_string()),
                                Value::String(crate::commands::format_node_key(&node_key)),
                                node,
                            ]
                        }
                        ConflictMap::Edges => {
                            let edge_key = EdgeKey::decode_fwd(&key).map_err(|e| e.to_string())?;
                            vec![
                                Value::String(edge_key.rtype().to_string()),
                                Value::String(crate::commands::format_edge_key(&edge_key)),
                                Value::Null,
                            ]
                        }
                        ConflictMap::Schema => vec![
                            Value::String("schema".to_string()),
                            Value::String(key.iter().map(|b| format!("{b:02x}")).collect()),
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

/// The `kind` yield column for a diff change.
fn change_kind(kind: acetone_graph::diff::ChangeKind) -> &'static str {
    use acetone_graph::diff::ChangeKind;
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
    }
}

/// Parse, bind and execute a query against a stored snapshot, resolving
/// any clause-group `AT <ref>` and any `CALL acetone.*` against the
/// repository.
fn execute_query(
    repo: &Repository,
    snapshot: &acetone_graph::Snapshot<'_>,
    cypher: &str,
) -> Result<QueryResult> {
    let nodes = snapshot.nodes().context("reading nodes")?;
    let edges = snapshot.edges().context("reading edges")?;
    let schema = snapshot.schema_entries().context("reading schema")?;

    let base = GraphSnapshot::from_records_with_schema(&nodes, &edges, &schema);
    let catalogue = catalogue_from_schema(schema);
    // Strict binding when the schema declares structure; a schema-free
    // repository (raw Phase 1 data) stays queryable under openCypher's
    // permissive read semantics. Recorded decision (bead acetone-yzc.6).
    let mode = if catalogue.is_empty() {
        BindMode::Lenient
    } else {
        BindMode::Strict
    };

    let parsed = acetone_cypher::parse(cypher).map_err(|e| anyhow!("{}", e.render(cypher)))?;
    let bound = acetone_cypher::bind::bind(cypher, &parsed, &catalogue, mode)
        .map_err(|e| anyhow!("{}", e.render(cypher)))?;
    let resolver = RepoResolver { repo, base };
    let procedures = RepoProcedures { repo };
    let result = acetone_cypher::exec::execute_versioned_with(
        &bound,
        &resolver,
        &procedures,
        &BTreeMap::new(),
    )
    .map_err(|e| anyhow!("{e}"))?;
    Ok(result)
}

// --- Rendering ---------------------------------------------------------------

fn render(result: &QueryResult, format: Format) {
    match format {
        Format::Table => render_table(result),
        Format::Json => render_json(result),
        Format::Csv => render_csv(result),
    }
}

fn render_table(result: &QueryResult) {
    if result.columns.is_empty() {
        outln!("(no columns)");
        return;
    }
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| row.iter().map(render_value).collect())
        .collect();
    let widths: Vec<usize> = result
        .columns
        .iter()
        .enumerate()
        .map(|(col, name)| {
            let body = cells
                .iter()
                .map(|row| row[col].chars().count())
                .max()
                .unwrap_or(0);
            body.max(name.chars().count())
        })
        .collect();

    let separator = |left: &str, mid: &str, right: &str| {
        let mut line = String::from(left);
        for (index, width) in widths.iter().enumerate() {
            line.push_str(&"─".repeat(width + 2));
            line.push_str(if index + 1 == widths.len() {
                right
            } else {
                mid
            });
        }
        line
    };

    outln!("{}", separator("┌", "┬", "┐"));
    outln!("{}", format_row(&result.columns, &widths));
    outln!("{}", separator("├", "┼", "┤"));
    for row in &cells {
        outln!("{}", format_row(row, &widths));
    }
    outln!("{}", separator("└", "┴", "┘"));
    outln!(
        "{} row{}",
        result.rows.len(),
        if result.rows.len() == 1 { "" } else { "s" }
    );
}

fn format_row(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::from("│");
    for (cell, width) in cells.iter().zip(widths) {
        line.push(' ');
        line.push_str(cell);
        line.push_str(&" ".repeat(width - cell.chars().count()));
        line.push_str(" │");
    }
    line
}

fn render_csv(result: &QueryResult) {
    outln!(
        "{}",
        result
            .columns
            .iter()
            .map(|c| csv_field(c))
            .collect::<Vec<_>>()
            .join(",")
    );
    for row in &result.rows {
        let line = row
            .iter()
            .map(|v| csv_field(&render_value(v)))
            .collect::<Vec<_>>()
            .join(",");
        outln!("{line}");
    }
}

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

fn render_json(result: &QueryResult) {
    outln!("[");
    for (index, row) in result.rows.iter().enumerate() {
        let fields: Vec<String> = result
            .columns
            .iter()
            .zip(row)
            .map(|(col, value)| format!("{}: {}", json_string(col), json_value(value)))
            .collect();
        let comma = if index + 1 == result.rows.len() {
            ""
        } else {
            ","
        };
        outln!("  {{{}}}{comma}", fields.join(", "));
    }
    outln!("]");
}

// --- Value rendering ---------------------------------------------------------

/// Human-readable rendering for table/CSV cells. Every string that
/// originates in the graph (property values, labels, relationship types,
/// property keys) is routed through [`sanitise_line`] — repository data
/// is attacker-writable (a hostile clone), and ANSI/C1 escape sequences
/// must never reach the terminal raw (the bar set by PR #25 for log/fsck
/// output). JSON output escapes separately in `json_string`.
fn render_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => acetone_cypher::exec::functions::format_float(*x),
        Value::String(s) => sanitise_line(s),
        Value::List(items) => {
            let inner = items
                .iter()
                .map(render_value)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{inner}]")
        }
        Value::Map(entries) => {
            let inner = entries
                .iter()
                .map(|(k, v)| format!("{}: {}", sanitise_line(k), render_value(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        Value::Node(node) => render_node(node),
        Value::Relationship(rel) => render_rel(rel),
        Value::Path(path) => format!("<path of {} rels>", path.rels.len()),
    }
}

fn render_node(node: &NodeValue) -> String {
    let labels: String = node
        .labels
        .iter()
        .map(|l| format!(":{}", sanitise_line(l)))
        .collect();
    if node.properties.is_empty() {
        format!("({labels})")
    } else {
        let props = node
            .properties
            .iter()
            .map(|(k, v)| format!("{}: {}", sanitise_line(k), render_value(v)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("({labels} {{{props}}})")
    }
}

fn render_rel(rel: &RelValue) -> String {
    format!("[:{}]", sanitise_line(&rel.rel_type))
}

/// JSON rendering (a minimal, dependency-free serialiser).
fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => {
            if x.is_finite() {
                acetone_cypher::exec::functions::format_float(*x)
            } else {
                // JSON has no NaN/Infinity; render as strings.
                json_string(&acetone_cypher::exec::functions::format_float(*x))
            }
        }
        Value::String(s) => json_string(s),
        Value::List(items) => {
            let inner = items.iter().map(json_value).collect::<Vec<_>>().join(", ");
            format!("[{inner}]")
        }
        Value::Map(entries) => {
            let inner = entries
                .iter()
                .map(|(k, v)| format!("{}: {}", json_string(k), json_value(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        Value::Node(node) => {
            let labels = node
                .labels
                .iter()
                .map(|l| json_string(l))
                .collect::<Vec<_>>()
                .join(", ");
            let props = node
                .properties
                .iter()
                .map(|(k, v)| format!("{}: {}", json_string(k), json_value(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{\"labels\": [{labels}], \"properties\": {{{props}}}}}")
        }
        Value::Relationship(rel) => format!("{{\"type\": {}}}", json_string(&rel.rel_type)),
        Value::Path(path) => format!("{{\"length\": {}}}", path.rels.len()),
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            // Escape all control characters: C0 (< 0x20), DEL (0x7f) and
            // the C1 range (0x80..=0x9f), matching sanitise_line's
            // coverage so no format leaks a raw terminal control.
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- Shell REPL --------------------------------------------------------------

/// A minimal readline REPL. Reads whole lines; a query may span lines and
/// is submitted when it ends in `;` or on a blank line.
pub fn shell(repo_path: &std::path::Path) -> Result<()> {
    let mut format = Format::Table;
    let stdin = io::stdin();
    let mut buffer = String::new();

    outln!("acetone shell — enter queries, ':quit' to exit, ':help' for commands");
    loop {
        let prompt = if buffer.is_empty() {
            "acetone> "
        } else {
            "      -> "
        };
        print!("{prompt}");
        io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.read_line(&mut line).context("reading input")? == 0 {
            outln!(); // EOF (Ctrl-D)
            break;
        }
        let trimmed = line.trim();

        // Meta-commands only at the start of a fresh query.
        if buffer.is_empty() && trimmed.starts_with(':') {
            match handle_meta(repo_path, trimmed, &mut format) {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => outln!("error: {e:#}"),
            }
            continue;
        }

        buffer.push_str(&line);
        let complete = trimmed.ends_with(';') || (trimmed.is_empty() && !buffer.trim().is_empty());
        if !complete {
            continue;
        }

        let query = buffer.trim().trim_end_matches(';').trim().to_string();
        buffer.clear();
        if query.is_empty() {
            continue;
        }
        match run_in_shell(repo_path, &query, format) {
            Ok(()) => {}
            Err(e) => outln!("error: {e:#}"),
        }
    }
    Ok(())
}

fn run_in_shell(repo_path: &std::path::Path, cypher: &str, format: Format) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    let snapshot = repo.workspace_snapshot()?;
    let result = execute_query(&repo, &snapshot, cypher)?;
    render(&result, format);
    Ok(())
}

/// Returns Ok(true) to quit the shell.
fn handle_meta(repo_path: &std::path::Path, line: &str, format: &mut Format) -> Result<bool> {
    let mut parts = line[1..].split_whitespace();
    let command = parts.next().unwrap_or("");
    match command {
        "quit" | "q" | "exit" => return Ok(true),
        "help" | "h" => {
            outln!(":checkout <ref> | :log | :format <table|json|csv> | :quit | :help");
        }
        "format" | "f" => match parts.next() {
            Some(name) => *format = Format::parse(name)?,
            None => outln!("current format: {format:?}"),
        },
        "checkout" => {
            let refspec = parts.next().context("usage: :checkout <ref>")?;
            let repo = Repository::open(repo_path)?;
            repo.checkout_branch(refspec)?;
            outln!("switched to {refspec}");
        }
        "log" => {
            let repo = Repository::open(repo_path)?;
            for entry in repo.log(None)? {
                // Commit subjects are repository-controlled (a hostile
                // clone); sanitise before the terminal, as the top-level
                // `log` command does (PR #25 bar).
                let subject = entry.message.lines().next().unwrap_or("");
                outln!("{} {}", entry.id.to_hex(), sanitise_line(subject));
            }
        }
        other => outln!("unknown command ':{other}' (:help for the list)"),
    }
    Ok(false)
}
