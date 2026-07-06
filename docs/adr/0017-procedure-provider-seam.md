# ADR-0017: Procedure-provider seam and the 8c3 split

*Status: accepted (agent decision, flagged for phase-boundary review) · Date: 2026-07-07 · Beads: acetone-8c3, acetone-b1v*

## Context

Spec §5.2 gives the read path four history procedures: `CALL acetone.log([ref])`,
`CALL acetone.diff(from, to)`, `CALL acetone.blame(label, key)`,
`CALL acetone.conflicts()`. They were parsed and bound (registered in
`bind::bound::PROCEDURES`, arity- and yield-checked) but the executor
rejected every `CALL` as unsupported.

The executor (`acetone-cypher`) deliberately does **not** depend on
`acetone-graph`: it reads a graph through the object-safe `GraphSource`
trait and resolves `AT <ref>` through `VersionResolver`, both supplied by
the caller. The procedures, though, need repository facilities that live
above this crate — the commit walk (`Repository::log`), the efficient prolly
diff (`Repository::diff`), blame, and the merge conflicts map. So the
executor needs a *seam* to call out, mirroring how `VersionResolver` already
lets it resolve refs without knowing about git.

acetone-8c3 also bundled a second deliverable: the queryable **virtual
graph** — `MATCH (n:_Added)` / `_Removed` / `_Modified` / `_Conflict` —
where diff/conflict elements appear as labelled nodes.

## Decision

### 1. A `ProcedureProvider` seam in the executor

```rust
pub trait ProcedureProvider {
    fn call(&self, name: &str, args: &[Value]) -> Result<Vec<Vec<Value>>, String>;
}
```

The executor evaluates the `CALL` arguments, invokes the provider, and binds
each returned tuple's declared yield columns — handling `YIELD` subsets and
reordering, `WHERE` post-filtering, standalone `CALL` (no `YIELD` →
declared columns become the result), and `YIELD` without `RETURN`. A
`NoProcedures` default errors on every call, keeping pure-executor callers
(unit tests, the TCK backend) working with no repository behind them.
`execute_versioned_with(query, resolver, procedures, params)` is the new
entry; `execute`/`execute_versioned`/`execute_write` pass `NoProcedures`.

The CLI (`acetone-cli`) supplies `RepoProcedures` over the open
`Repository`, so `CALL acetone.diff` and the `acetone diff` command compute
from the **same** `Repository::diff` — one prolly-diff implementation, no
divergence between the two surfaces (verified by an end-to-end test).

### 2. 8c3 splits: seam + diff/log now; virtual graph later (acetone-b1v)

This bead ships the seam and the two procedures whose data already exists:

- `acetone.log([ref])` → `(commit, subject)` rows;
- `acetone.diff(from, to)` → `(kind, label, key)` rows.

`acetone.blame` and `acetone.conflicts` are wired through the seam but return
a clear "not yet available" error: their data is owned by **acetone-14c.6**
(blame) and **acetone-14c.4** (the persisted conflicts map), which are not
yet built.

The **virtual graph** (`MATCH (n:_Added)` …) is split into **acetone-b1v**
because:

- its **invocation surface is underspecified** — spec §5.2 shows the
  procedure "yielding change rows and, in graph form, `_Added`/… virtual
  elements", and §9 only says to keep the executor's `LabelScan`
  provider-pluggable. How a bare `MATCH (n:_Added)` obtains its `(from, to)`
  diff context, and how `_Conflict` binds to the current merge, is an
  **API-shape decision** that warrants its own ADR and Greg's eye;
- its `_Conflict` half depends on acetone-14c.4 (unbuilt).

Shipping the fully-specified, unblocked seam + procedures now — rather than
guessing the virtual-graph surface — keeps this change reviewable and avoids
baking a query-language surface unilaterally.

## Consequences

- `CALL acetone.log`/`acetone.diff` work from `acetone query` and the shell,
  sharing the repository's history with the CLI commands.
- The seam is the shared infrastructure acetone-14c.4 (conflicts), 14c.6
  (blame) and b1v (virtual graph) extend — each adds a provider case or a
  `LabelScan` source without touching the executor's clause pipeline.
- `resolve_commit` still does not accept git ancestry syntax (`main~1`), so
  `CALL acetone.diff('main~1', 'main')` fails to resolve; that is a
  pre-existing refspec limitation, filed separately, not introduced here.
- **Flagged for the Phase 4 report:** the 8c3 split and the deferred
  virtual-graph API decision (acetone-b1v needs an ADR before implementation).
