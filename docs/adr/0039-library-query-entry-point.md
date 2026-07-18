# ADR-0039: A library-level Cypher query entry point (`Session`)

- Status: accepted — ratified by Greg at the Phase 7 / 0.2 boundary review (2026-07-18)
- Date: 2026-07-16
- Deciders: agent under the Phase 7 mandate (frozen-0.2 API shape recorded by ADR and flagged for the phase-boundary review; the API shape is a clear default, the layering resolution is the notable decision)
- Related: beads `acetone-vf6` (this work), `acetone-ijq` (route the whole CLI through the `acetone-core` façade — separate, mechanical), `acetone-cbl.5` (PR #76 review that raised it); ADR-0036 (`QueryLimits`), spec §5 (query), §7 (library/CLI)

## Context

Running an openCypher query end-to-end — parse → bind → build the
`GraphSnapshot`/`VersionResolver`/`ProcedureProvider` adapters over the
repository → execute → (for a write) persist and save — lived **only** in
`crates/acetone-cli/src/query.rs` (~150 lines). `acetone-core` advertises itself
as "the single dependency a library consumer adds", yet it exposed no way to run
a query: a library consumer had to re-implement all of that glue, including the
non-obvious `RepoResolver` (serves clause-group `AT <ref>`) and `RepoProcedures`
(serves `CALL acetone.log/diff/blame/conflicts`).

This must be a proper library API before `acetone-core`'s query surface is frozen
at the 0.2 gate.

### The layering constraint (the decision the bead left open)

The bead suggested "acetone-graph (or acetone-core)" and the obvious shape is
`Repository::query(cypher)`. But the workspace has **strictly downward
dependencies**: `acetone-cypher → acetone-graph`, and `acetone-graph` does *not*
depend on `acetone-cypher`. A `query()` method on `Repository` (which lives in
`acetone-graph`) would force an **upward** `graph → cypher` dependency, inverting
the layering and creating a cycle. So the entry point cannot live on
`Repository`.

## Decision

Add a `session` module to **`acetone-cypher`** — the lowest crate that already
depends on both the executor and `acetone-graph` (it hosts `execute_write` and
`persist_changes`, which already operate on graph `Transaction`/`Snapshot`
types). `acetone-core` re-exports it, so the façade is genuinely sufficient.

The API, frozen at 0.2:

```rust
pub struct Session<'r> { /* &Repository */ }

impl<'r> Session<'r> {
    pub fn new(repo: &'r Repository) -> Self;
    /// Auto-dispatch: a read runs against the workspace snapshot; a write runs
    /// in a single-writer transaction and advances the workspace atomically.
    pub fn run(&self, cypher: &str) -> Result<Outcome, QueryError>;
    /// As `run`, with query parameters and an explicit governor budget.
    pub fn run_with(&self, cypher: &str, params: &BTreeMap<String, Value>,
                    limits: &QueryLimits) -> Result<Outcome, QueryError>;
    /// Read-only against a past version (`--at`); a write query is rejected.
    pub fn query_at(&self, cypher: &str, refspec: &str) -> Result<QueryResult, QueryError>;
}

pub enum Outcome { Read(QueryResult), Write(QueryResult) } // Write: workspace already saved

pub enum QueryError { Parse(..), Bind(..), Exec(..), Persist(..), Graph(..), WriteAtVersion }
impl QueryError { pub fn render(&self, cypher: &str) -> String; } // caret diagnostics
```

The `RepoResolver` and `RepoProcedures` adapters move out of the CLI into this
module unchanged in behaviour; the few CLI key-formatting helpers they used
(`format_node_key`/`format_edge_key`/`parse_value`) are thin wrappers over
`acetone_model::display`, so they are replaced with direct `display` calls plus
the inlined int-or-string key heuristic. Binding is `Strict` when the schema
declares structure and `Lenient` for a schema-free repository (the recorded
decision from `acetone-yzc.6`), exactly as the CLI did.

The CLI keeps only what is genuinely presentation: `Format`, `render`,
`render_write_summary`, the row cap. Its `run` and shell handler call the
`Session` and map `QueryError` to `anyhow` via `render(cypher)`; the read/write
rendering distinction comes from the `Outcome` variant (no re-parse). Wiring the
*rest* of the CLI through `acetone-core` is the separate bead `acetone-ijq`.

## Consequences

- `acetone-core` gains a real query API; a library consumer runs Cypher —
  reads, writes, `AT <version>`, and `CALL acetone.*` — without re-implementing
  any glue.
- The orchestration has one home instead of two; the CLI's query path shrinks by
  ~150 lines and both the one-shot command and the REPL share the library path,
  so behaviour cannot drift between them.
- `QueryError` is a typed, `thiserror`-based library error (libraries use
  `thiserror`; only the CLI uses `anyhow`) with a `render` that reproduces the
  caret diagnostics, so a library consumer gets the **same parse/bind/execution
  diagnostics** the CLI shows. Store-level failures (`Graph`/`Persist`) render as
  their `Display` message: the underlying `GraphError` already names the
  offending ref/operation, so the CLI's former `anyhow` context prefixes
  (`"reading the workspace"`, `"reading at <ref>"`, …) are intentionally dropped
  — the "same messages" guarantee is scoped to query diagnostics, not I/O
  wrapper text.
- Pure orchestration: no encoding, root-hash, merge or format change — Load-
  Bearing Invariants 1–5 are untouched, no `format_version` bump.
- The write path saves the workspace atomically (matching the CLI); it does not
  auto-*commit*. Exposing the transaction for staged multi-statement writes is a
  possible later refinement, out of scope here.
- Rejected: `Repository::query()` (inverts the crate layering); putting the
  orchestration only in `acetone-core` (a library consumer of `acetone-cypher`
  alone — a legitimate lighter dependency — would still lack it, and the glue
  belongs with the executor it drives).
