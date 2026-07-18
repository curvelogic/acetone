# ADR-0043: Query advisories — the schema-free undeclared-label note

- Status: accepted — ratified by Greg at the Phase 7 / 0.2 boundary review (2026-07-18)
- Date: 2026-07-16
- Bead: acetone-7bn.5 (0.1.1 sweep Tier 1, deferred to Phase 7)
- Relates to: acetone-7bn.4 (did-you-mean on unknown label), ADR-0039 (library query entry point), the `yzc.6` schema-free read decision.

## Context

In a repository **with** a declared schema, an undeclared label in a `MATCH`
is a hard bind error with a "did you mean …?" suggestion (acetone-7bn.4). But a
**schema-free** repository binds in `BindMode::Lenient` (openCypher's permissive
read semantics, decision `yzc.6`): an undeclared label is *not* an error, so
`MATCH (n:Nope) RETURN n` returns 0 rows with no signal that `Nope` is unknown.
A dogfooding user reads the empty table as "my query is wrong" with no clue — an
exploration trap (confirmed live 2026-07-13).

Making it an error was rejected: it would break legitimate schema-free
exploration and openCypher conformance. The fix must **not** change result
semantics (still 0 rows, still exit 0).

## Decision

Introduce **query advisories**: non-error, non-semantic diagnostics attached to
a query result.

- `QueryResult` (the frozen 0.2 library type, ADR-0039) gains
  `advisories: Vec<String>`. The executor leaves it empty; the **session** layer
  — which holds the schema catalogue — populates it after execution. It never
  affects `rows`, `columns`, `stats`, or the error path.
- The first advisory: in `Lenient` (schema-free) mode, a read that produced **no
  rows** and referenced a label that matches **no node in the graph** yields one
  note naming that label and how to declare one. It fires only on an empty
  result (the trap), only in schema-free mode (a declared schema already
  errors), only when a label was actually referenced (a label-free `MATCH (n)`
  gets none), and — checked with the executor's own `nodes_by_labels` — only for
  a genuinely absent label, never for a real, populated label whose `WHERE`
  filtered the result to empty (which would make "check for a typo" misleading).
  The graph check runs only on this narrow schema-free-and-empty path.
- **Surfacing**: the CLI prints advisories to **stderr**, so they never pollute
  the result on stdout (table/JSON/CSV) and never change the exit status. Library
  callers read `result.advisories` and choose their own presentation.

Advisories are a general channel; this ADR seeds it with one note. Future
non-error diagnostics (e.g. an unindexed scan warning) can reuse it without
further API change.

## Consequences

- The 0.2 library API surface gains one additive field on `QueryResult`
  (backward compatible; done now, during the freeze, so it is part of 0.2).
- Result semantics and exit status are unchanged — a scripted `MATCH … RETURN`
  piped to a file still receives exactly its rows on stdout; the note is
  out-of-band on stderr.
- The advisory is intentionally conservative (schema-free + empty result +
  label referenced + label absent from the graph). It does not fire for a
  schema-free `MATCH` that *did* match, for a populated label a `WHERE` filtered
  to empty, nor for `OPTIONAL MATCH` (which yields a null row, not an empty
  result). Labels inside a `WHERE` sub-pattern are not scanned, and extending
  advisories to the write path or to undeclared relationship types (already a
  hard error today) is left open.
