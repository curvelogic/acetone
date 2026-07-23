//! The `query` command and `shell` REPL (spec §5, §7): parse → bind →
//! execute an openCypher read query against a stored graph version, and
//! render the result.

use std::io::{self, IsTerminal};

use acetone_core::cypher::exec::QueryResult;
use acetone_core::cypher::exec::value::{NodeValue, RelValue, Value};
use acetone_core::cypher::session::{Outcome as QueryOutcome, Session};
use acetone_core::graph::Repository;
use anyhow::{Context, Result, anyhow};

use unicode_width::UnicodeWidthStr;

use crate::output::{errln, outln};
use crate::value::{sanitise_identifier, sanitise_line};

/// Row cap applied to `--format table` output **in the interactive shell
/// only** (spec: a large `MATCH (n) RETURN n` should not flood the terminal).
/// The one-shot `acetone query` command is never capped, so a scripted
/// `query --format table` piped to a file gets every row.
const SHELL_ROW_CAP: usize = 1000;

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

/// Run one query and print the result. Orchestration (parse → bind → execute →
/// for a write, persist and save) lives in the library [`Session`] (ADR-0039);
/// the CLI keeps only presentation.
pub fn run(
    repo_path: &std::path::Path,
    cypher: &str,
    at: Option<&str>,
    format: Format,
) -> Result<()> {
    let repo = Repository::open(repo_path).context("opening repository")?;
    let session = Session::new(&repo);
    match at {
        // A read against a past version; a write with `--at` is rejected.
        Some(refspec) => {
            let result = session
                .query_at(cypher, refspec)
                .map_err(|e| at_error(e, cypher))?;
            // One-shot command: never cap (a scripted `query --format table`
            // piped to a file must get every row).
            render(&result, format, None);
        }
        None => {
            let outcome = session
                .run(cypher)
                .map_err(|e| anyhow!("{}", e.render(cypher)))?;
            render_outcome(&outcome, format, None);
        }
    }
    Ok(())
}

/// Render a query outcome: the rows, plus a write summary when a write ran.
fn render_outcome(outcome: &QueryOutcome, format: Format, max_rows: Option<usize>) {
    render(outcome.result(), format, max_rows);
    if outcome.is_write() {
        render_write_summary(&outcome.result().stats);
    }
    // Non-error advisories (e.g. a schema-free MATCH on an undeclared label that
    // matched nothing, acetone-7bn.5) go to stderr, so they never pollute the
    // result on stdout or change the exit status. The text is acetone-generated
    // with label names debug-escaped, but route it through sanitise_line too.
    for note in &outcome.result().advisories {
        errln!("{}", sanitise_line(note));
    }
}

/// Map a `--at` query error, giving the flag-specific hint for a write attempt.
fn at_error(error: acetone_core::cypher::session::QueryError, cypher: &str) -> anyhow::Error {
    match error {
        acetone_core::cypher::session::QueryError::WriteAtVersion => {
            anyhow!("cannot write with --at: writes target the workspace, not a past version")
        }
        other => anyhow!("{}", other.render(cypher)),
    }
}

/// A one-line summary of a write's side effects (openCypher counts).
fn render_write_summary(stats: &acetone_core::cypher::exec::WriteSummary) {
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

// --- Rendering ---------------------------------------------------------------

/// Render a result. `max_rows` caps how many rows the table renderer prints
/// (with a notice for the remainder); `None` means "all rows". Only the
/// interactive shell passes `Some(_)` — one-shot `acetone query` never caps.
fn render(result: &QueryResult, format: Format, max_rows: Option<usize>) {
    match format {
        Format::Table => render_table(result, max_rows),
        Format::Json => render_json(result),
        Format::Csv => render_csv(result),
    }
}

fn render_table(result: &QueryResult, max_rows: Option<usize>) {
    if result.columns.is_empty() {
        outln!("(no columns)");
        return;
    }
    let cells: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(column, value)| render_cell(value, result, column))
                .collect()
        })
        .collect();
    // Only the first `shown` rows are printed; the true total drives the
    // `N row(s)` line and the "more rows" notice.
    let total = cells.len();
    let shown = max_rows.map_or(total, |cap| total.min(cap));
    // Column width is the maximum *display* width (Unicode TR#11) over the
    // header and the visible cells — char count is wrong for CJK/emoji (2
    // cells) and combining marks (0), which would misalign the borders.
    let widths: Vec<usize> = result
        .columns
        .iter()
        .enumerate()
        .map(|(col, name)| {
            let body = cells[..shown]
                .iter()
                .map(|row| UnicodeWidthStr::width(row[col].as_str()))
                .max()
                .unwrap_or(0);
            body.max(UnicodeWidthStr::width(name.as_str()))
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
    for row in &cells[..shown] {
        outln!("{}", format_row(row, &widths));
    }
    outln!("{}", separator("└", "┴", "┘"));
    if shown < total {
        outln!(
            "… {} more row{} (showing first {}; use --format csv or json for all)",
            total - shown,
            if total - shown == 1 { "" } else { "s" },
            shown
        );
    }
    outln!("{total} row{}", if total == 1 { "" } else { "s" });
}

fn format_row(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::from("│");
    for (cell, width) in cells.iter().zip(widths) {
        // Pad to the *display* width: a cell of display-width `w` in a column
        // of width `W` gets `W - w` trailing spaces (never a char-count diff).
        let cell_width = UnicodeWidthStr::width(cell.as_str());
        line.push(' ');
        line.push_str(cell);
        line.push_str(&" ".repeat(width.saturating_sub(cell_width)));
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
            .enumerate()
            .map(|(column, v)| csv_field(&render_cell(v, result, column)))
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

/// One table/CSV cell: columns the executor flagged as identifier-shaped
/// (`labels(n)`, `keys(n)`, `type(r)`, procedure identifier yields —
/// acetone-0ds) take the stricter identifier escaping; everything else takes
/// the ordinary value rendering. JSON output ignores the flag ([`json_value`]
/// keeps the raw round-trip).
fn render_cell(value: &Value, result: &QueryResult, column: usize) -> String {
    if result
        .identifier_columns
        .get(column)
        .copied()
        .unwrap_or(false)
    {
        render_identifier_value(value)
    } else {
        render_value(value)
    }
}

/// Rendering for an identifier-flagged cell: plain strings — and strings
/// inside lists, e.g. a `labels(n)` cell — are escaped with
/// [`sanitise_identifier`] (zero-width/invisible characters included);
/// every other shape falls back to [`render_value`].
fn render_identifier_value(value: &Value) -> String {
    match value {
        Value::String(s) => sanitise_identifier(s),
        Value::List(items) => {
            let inner = items
                .iter()
                .map(render_identifier_value)
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{inner}]")
        }
        other => render_value(other),
    }
}

/// Human-readable rendering for table/CSV cells. Every string that
/// originates in the graph (property values, labels, relationship types,
/// property keys) is routed through [`sanitise_line`] — repository data
/// is attacker-writable (a hostile clone), and ANSI/C1 escape sequences
/// must never reach the terminal raw (the bar set by PR #25 for log/fsck
/// output). JSON output escapes separately in `json_string`.
fn render_value(value: &Value) -> String {
    match value {
        // A distinct marker so a genuine NULL is not confused with a string
        // whose contents are "null" (which renders as itself). Trade-off: a
        // string literally spelled "NULL" still collides with this marker —
        // acceptable, and only in table/CSV; `--format json` uses unambiguous
        // JSON `null` via `json_value` and is unaffected.
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => acetone_core::cypher::exec::functions::format_float(*x),
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
                .map(|(k, v)| format!("{}: {}", sanitise_identifier(k), render_value(v)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        Value::Node(node) => render_node(node),
        Value::Relationship(rel) => render_rel(rel),
        Value::Path(path) => format!("<path of {} rels>", path.rels.len()),
        // A read carrier renders exactly as its string form would (ADR-0038):
        // hex for Bytes, a stable debug string for temporals, sanitised.
        Value::Stored(mv) => sanitise_line(&acetone_core::cypher::exec::value::render_stored(mv)),
    }
}

fn render_node(node: &NodeValue) -> String {
    // Labels and property keys are identifier-shaped: escaped to the
    // stricter bar (zero-width included), unlike the property values.
    let labels: String = node
        .labels
        .iter()
        .map(|l| format!(":{}", sanitise_identifier(l)))
        .collect();
    if node.properties.is_empty() {
        format!("({labels})")
    } else {
        let props = node
            .properties
            .iter()
            .map(|(k, v)| format!("{}: {}", sanitise_identifier(k), render_value(v)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("({labels} {{{props}}})")
    }
}

fn render_rel(rel: &RelValue) -> String {
    format!("[:{}]", sanitise_identifier(&rel.rel_type))
}

/// JSON rendering (a minimal, dependency-free serialiser).
fn json_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => {
            if x.is_finite() {
                acetone_core::cypher::exec::functions::format_float(*x)
            } else {
                // JSON has no NaN/Infinity; render as strings.
                json_string(&acetone_core::cypher::exec::functions::format_float(*x))
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
        // Carried as its string rendering, JSON-escaped like any string (ADR-0038).
        Value::Stored(mv) => json_string(&acetone_core::cypher::exec::value::render_stored(mv)),
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
            // Escape everything unsafe for the terminal: the C0 (< 0x20),
            // DEL (0x7f) and C1 (0x80..=0x9f) controls, plus the bidirectional
            // formatting overrides — matching `sanitise_line`'s coverage so no
            // format leaks a raw terminal control or a Trojan-source reorder.
            c if crate::value::is_unsafe_for_display(c) => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- Shell REPL --------------------------------------------------------------

/// Whether processing an input line should end the session.
enum Outcome {
    Quit,
    Continue,
}

/// The interactive Cypher REPL. When stdin is a terminal it uses `rustyline`
/// for line editing, history and arrow-key recall; when stdin is piped (a
/// script, or a test) it falls back to plain line reading so the shell stays
/// scriptable. A query may span lines and is submitted when a line ends in
/// `;` or on a blank line; meta-commands (`:help`, `:declare-*`, `:commit`,
/// …) are handled at the start of a fresh statement, and `:quit`/`:cancel`
/// work mid-statement too.
pub fn shell(repo_path: &std::path::Path) -> Result<()> {
    let mut format = Format::Table;
    let mut buffer = String::new();

    if !io::stdin().is_terminal() {
        // Non-interactive: read lines plainly, no editing/history/prompts.
        let stdin = io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            if stdin.read_line(&mut line).context("reading input")? == 0 {
                flush_pending(repo_path, &mut buffer, &mut format); // run an unterminated final statement
                break; // EOF
            }
            if let Outcome::Quit = process_shell_line(repo_path, &mut buffer, &mut format, &line) {
                break;
            }
        }
        return Ok(());
    }

    outln!("acetone shell — enter queries, ':quit' to exit, ':help' for commands");
    let config = rustyline::Config::builder()
        .max_history_size(1000)
        .context("configuring the line editor")?
        .build();
    let mut editor: rustyline::Editor<(), rustyline::history::FileHistory> =
        rustyline::Editor::with_config(config)
            .context("initialising the interactive line editor")?;
    let history = shell_history_path();
    if let Some(path) = &history {
        let _ = editor.load_history(path);
    }
    loop {
        let prompt = shell_prompt(repo_path, buffer.is_empty());
        match editor.readline(&prompt) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = editor.add_history_entry(line.as_str());
                }
                if let Outcome::Quit =
                    process_shell_line(repo_path, &mut buffer, &mut format, &line)
                {
                    break;
                }
            }
            // Ctrl-C: abandon the current (partial) statement, stay in the shell.
            Err(rustyline::error::ReadlineError::Interrupted) => {
                buffer.clear();
                outln!("(cancelled — Ctrl-D to exit)");
            }
            // Ctrl-D: run an unterminated final statement, then exit.
            Err(rustyline::error::ReadlineError::Eof) => {
                flush_pending(repo_path, &mut buffer, &mut format);
                break;
            }
            Err(e) => {
                errln!("error: {e}");
                break;
            }
        }
    }
    if let Some(path) = &history {
        let _ = editor.save_history(path);
    }
    Ok(())
}

/// Per-user history file for the interactive shell.
fn shell_history_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".acetone_history"))
}

/// The prompt for the next line: branch-aware (with a `*` dirty marker) at the
/// start of a statement, aligned continuation otherwise. Best-effort — falls
/// back to a plain prompt if the repo cannot be read.
fn shell_prompt(repo_path: &std::path::Path, fresh: bool) -> String {
    if !fresh {
        return "      -> ".to_string();
    }
    match Repository::open(repo_path) {
        Ok(repo) => {
            let branch = repo
                .current_branch()
                .ok()
                .flatten()
                .map(|full| {
                    repo.namespace()
                        .branch_name(&full)
                        .unwrap_or(full.as_str())
                        .to_owned()
                })
                .unwrap_or_else(|| "detached".to_string());
            let mark = if repo.is_dirty().unwrap_or(false) {
                "*"
            } else {
                ""
            };
            // Ref-name validation already forbids control bytes, but sanitise
            // the branch defensively — the prompt is repository-controlled,
            // identifier-shaped text (zero-width spoofing included).
            format!("acetone:{}{mark}> ", sanitise_identifier(&branch))
        }
        Err(_) => "acetone> ".to_string(),
    }
}

/// Process one input line against the accumulating statement buffer: dispatch
/// a meta-command, accumulate a partial statement, or run a completed one.
fn process_shell_line(
    repo_path: &std::path::Path,
    buffer: &mut String,
    format: &mut Format,
    raw: &str,
) -> Outcome {
    let line = raw.trim_end_matches(['\n', '\r']);
    let trimmed = line.trim();

    // Meta-commands: at the start of a fresh statement, or `:quit`/`:cancel`
    // any time (so you can escape a half-typed statement).
    if let Some(body) = trimmed.strip_prefix(':') {
        let cmd = body.split_whitespace().next().unwrap_or("");
        let escapes = matches!(cmd, "quit" | "q" | "exit" | "cancel");
        if buffer.is_empty() || escapes {
            match handle_meta(repo_path, trimmed, format, buffer) {
                Ok(true) => return Outcome::Quit,
                Ok(false) => {}
                // Errors go to stderr so they never interleave with result
                // output on stdout (informational meta output stays on stdout).
                Err(e) => errln!("error: {e:#}"),
            }
            return Outcome::Continue;
        }
    }

    // A whitespace-only line at a fresh prompt: ignore (do not enter a
    // continuation with an empty pending statement).
    if buffer.is_empty() && trimmed.is_empty() {
        return Outcome::Continue;
    }

    buffer.push_str(line);
    buffer.push('\n');
    let complete = trimmed.ends_with(';') || (trimmed.is_empty() && !buffer.trim().is_empty());
    if !complete {
        return Outcome::Continue;
    }

    let query = buffer.trim().trim_end_matches(';').trim().to_string();
    buffer.clear();
    if query.is_empty() {
        return Outcome::Continue;
    }
    if let Err(e) = run_in_shell(repo_path, &query, *format) {
        // Query errors go to stderr, keeping stdout as the pure result stream.
        errln!("error: {e:#}");
    }
    Outcome::Continue
}

/// Run any unterminated statement still in the buffer — called at EOF so a
/// piped statement with no trailing `;` (e.g. `printf 'RETURN 1' | acetone
/// shell`) still executes rather than being silently dropped.
fn flush_pending(repo_path: &std::path::Path, buffer: &mut String, format: &mut Format) {
    if !buffer.trim().is_empty() {
        // A blank line is already a statement terminator; reuse that path.
        let _ = process_shell_line(repo_path, buffer, format, "");
    }
}

fn run_in_shell(repo_path: &std::path::Path, cypher: &str, format: Format) -> Result<()> {
    let repo = Repository::open(repo_path)?;
    // The library `Session` dispatches read vs write: a write goes through the
    // transactional path and advances the workspace (so subsequent shell queries
    // see it), exactly as `run` does — the shell never silently executes the read
    // side of a write. The user commits separately with `acetone commit`.
    let outcome = Session::new(&repo)
        .run(cypher)
        .map_err(|e| anyhow!("{}", e.render(cypher)))?;
    render_outcome(&outcome, format, Some(SHELL_ROW_CAP));
    Ok(())
}

/// Split a meta-command's argument string into leading positionals and
/// `--flag value...` groups (a forgiving mini-parser mirroring the CLI verbs;
/// values are whitespace-split, so identifiers only — no quoting).
fn parse_meta_args(rest: &str) -> (Vec<String>, std::collections::BTreeMap<String, Vec<String>>) {
    let mut positional = Vec::new();
    let mut flags: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let mut current: Option<String> = None;
    for tok in rest.split_whitespace() {
        if let Some(flag) = tok.strip_prefix("--") {
            current = Some(flag.to_string());
            flags.entry(flag.to_string()).or_default();
        } else if let Some(flag) = &current {
            flags.entry(flag.clone()).or_default().push(tok.to_string());
        } else {
            positional.push(tok.to_string());
        }
    }
    (positional, flags)
}

/// Handle a `:meta` command. Returns Ok(true) to quit the shell. Errors are
/// reported to the caller and never end the session. `buffer` is the
/// accumulating statement (cleared by `:cancel`).
fn handle_meta(
    repo_path: &std::path::Path,
    line: &str,
    format: &mut Format,
    buffer: &mut String,
) -> Result<bool> {
    let body = &line[1..];
    let (command, rest) = match body.find(char::is_whitespace) {
        Some(i) => (&body[..i], body[i..].trim()),
        None => (body, ""),
    };
    match command {
        "quit" | "q" | "exit" => return Ok(true),
        "cancel" => {
            buffer.clear();
            outln!("(cancelled)");
        }
        "help" | "h" => {
            outln!("Meta-commands:");
            outln!("  :help, :h                     show this help");
            outln!("  :quit, :q, :exit              leave the shell (or Ctrl-D)");
            outln!("  :cancel                       discard the half-typed statement (or Ctrl-C)");
            outln!("  :status                       branch, head and workspace state");
            outln!("  :commit <message>             commit the workspace");
            outln!("  :checkout <ref>               switch to a branch or version");
            outln!("  :log                          commit history, newest first");
            outln!("  :schema [--at <ref>]          the declared schema");
            outln!("  :declare-label <L> --key <p>... [--require <p>...] [--unique <p>...]");
            outln!("  :declare-rel-type <TYPE>");
            outln!("  :declare-index <name> --label <L> --property <p>...");
            outln!("  :format, :f <table|json|csv>  result format");
            outln!("End a statement with ';' or a blank line.");
        }
        "format" | "f" => {
            if rest.is_empty() {
                outln!("current format: {format:?}");
            } else {
                *format = Format::parse(rest)?;
            }
        }
        "status" => crate::commands::status(repo_path, false)?,
        "commit" => {
            if rest.is_empty() {
                anyhow::bail!("usage: :commit <message>");
            }
            crate::commands::commit(repo_path, rest, &[])?;
        }
        "schema" => {
            let (_pos, flags) = parse_meta_args(rest);
            let at = flags.get("at").and_then(|v| v.first()).map(String::as_str);
            crate::commands::schema(repo_path, at, false)?;
        }
        "declare-label" => {
            let (pos, flags) = parse_meta_args(rest);
            let label = pos
                .first()
                .context("usage: :declare-label <LABEL> --key <prop>...")?;
            let key = flags.get("key").cloned().unwrap_or_default();
            if key.is_empty() {
                anyhow::bail!("usage: :declare-label <LABEL> --key <prop>...");
            }
            let require = flags.get("require").cloned().unwrap_or_default();
            let unique = flags.get("unique").cloned().unwrap_or_default();
            crate::commands::declare_label(repo_path, label, &key, &require, &unique)?;
        }
        "declare-rel-type" => {
            if rest.is_empty() {
                anyhow::bail!("usage: :declare-rel-type <TYPE>");
            }
            crate::commands::declare_rel_type(repo_path, rest)?;
        }
        "declare-index" => {
            let (pos, flags) = parse_meta_args(rest);
            let name = pos
                .first()
                .context("usage: :declare-index <name> --label <L> --property <p>...")?;
            let label = flags
                .get("label")
                .and_then(|v| v.first())
                .context("usage: :declare-index <name> --label <L> --property <p>...")?;
            let props = flags.get("property").cloned().unwrap_or_default();
            if props.is_empty() {
                anyhow::bail!("usage: :declare-index <name> --label <L> --property <p>...");
            }
            crate::commands::declare_index(repo_path, name, label, &props)?;
        }
        "checkout" => {
            let refspec = rest
                .split_whitespace()
                .next()
                .context("usage: :checkout <ref>")?;
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

#[cfg(test)]
mod tests {
    use super::json_string;

    #[test]
    fn json_string_escapes_controls_and_bidi() {
        // Quotes/backslash/whitespace controls take their short escapes.
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        // A right-to-left override (Trojan source) is not is_control(), but
        // must still be escaped to the JSON \uXXXX form, never emitted raw.
        let out = json_string("safe\u{202e}reversed");
        assert!(!out.contains('\u{202e}'));
        assert_eq!(out, "\"safe\\u202ereversed\"");
        // Legitimate non-ASCII, including emoji ZWJ sequences, passes through.
        assert_eq!(json_string("déjà 👩‍👧"), "\"déjà 👩‍👧\"");
    }
}
