//! The `query` command and `shell` REPL (spec §5, §7): parse → bind →
//! execute an openCypher read query against a stored graph version, and
//! render the result.

use std::collections::BTreeMap;
use std::io::{self, Write};

use acetone_cypher::bind::BindMode;
use acetone_cypher::exec::value::{NodeValue, RelValue, Value};
use acetone_cypher::exec::{GraphSnapshot, QueryResult, catalogue_from_schema, execute};
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
    let snapshot = match at {
        Some(refspec) => repo
            .snapshot(refspec)
            .with_context(|| format!("reading at {refspec:?}"))?,
        None => repo.workspace_snapshot().context("reading the workspace")?,
    };
    let result = execute_query(&snapshot, cypher)?;
    render(&result, format);
    Ok(())
}

/// Parse, bind and execute a query against a stored snapshot.
fn execute_query(snapshot: &acetone_graph::Snapshot<'_>, cypher: &str) -> Result<QueryResult> {
    let nodes = snapshot.nodes().context("reading nodes")?;
    let edges = snapshot.edges().context("reading edges")?;
    let schema = snapshot.schema_entries().context("reading schema")?;

    let graph = GraphSnapshot::from_records(&nodes, &edges);
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
    let result = execute(&bound, &graph, &BTreeMap::new()).map_err(|e| anyhow!("{e}"))?;
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
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
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
    let result = execute_query(&snapshot, cypher)?;
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
                let subject = entry.message.lines().next().unwrap_or("");
                outln!("{} {}", entry.id.to_hex(), subject);
            }
        }
        other => outln!("unknown command ':{other}' (:help for the list)"),
    }
    Ok(false)
}
