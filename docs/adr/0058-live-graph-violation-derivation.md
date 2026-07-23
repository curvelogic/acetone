# ADR-0058: Graph violations are re-derived live and named at completion

- Status: accepted (agent decision, flagged for phase-boundary review)
- Date: 2026-07-23
- Bead: acetone-jm8
- Supersedes the "known boundary: persisted graph-violation entries are not
  re-derived after a repair" slice of ADR-0041.

## Context

PR #178's manual verification hit a silent gap. A merge combining a property
cell conflict with a latent dangling edge (theirs deletes a node, ours adds an
edge to it) reports **only** the cell conflict at merge time — correctly, per
ADR-0016: cell conflicts short-circuit graph validation because the merged
graph is partial, so cell and graph conflicts never coexist in one outcome.
But after `resolve --all-theirs`, the resolution path dropped the `conflicts`
map entirely: `Repository::conflicts()` and `CALL acetone.conflicts()`
returned nothing, `status` said "all conflicts resolved", and the completion
commit was refused by ADR-0041's re-validation with an anonymous string —
only `fsck` could name the dangling edge. Spec §7 requires violations to be
"structured conflicts, not errors".

Two further mismatches: for a *pure* graph-violation merge the persisted
marker entries were skipped by `acetone.conflicts` (a stale "not persisted"
comment predating ADR-0041), and ADR-0041 itself noted the persisted entries
"can over-report until the completion commit clears them" because a repair
write never updates them.

## Decision

1. **Graph violations are re-derived live, not read from the map.** While a
   merge is in progress and **no cell conflicts remain** (the graph is
   complete — ADR-0016's precondition), `Repository::conflicts()` runs the
   same pure `validate_merged(merge base, workspace)` that gates merge and
   completion, and reports each breach as
   `WorkspaceConflict::Graph(GraphViolation)`. While cell conflicts remain,
   only they are reported (a partial graph is not validated). The persisted
   graph entries stay as advisory markers with an unchanged encoding — no
   format change; their violation details are simply never decoded.

2. **The completion refusal names each violation.** `Transaction::commit`
   raises `GraphError::MergeViolations(Vec<GraphViolation>)` instead of an
   anonymous message; its rendering names each violation (bounded at eight,
   then a count), sharing one `Display` with the CLI merge report, and
   escaping attacker-writable labels/keys via `acetone_model::display`.

3. **`CALL acetone.conflicts()` gains a leading `kind` column** (`cell`,
   `dangling-edge`, `missing-required`, `unique`), yielding one row per
   violation (one per colliding node for UNIQUE); `property` carries the
   missing/UNIQUE property or which endpoint (`src`/`dst`) of a dangling
   relationship is absent. Existing `YIELD`-by-name queries are unaffected.

## Consequences

- The operator experience matches the design record: a violation the merge
  composed *or a resolution introduced* is visible as structured conflict
  data before commit refuses it, and the refusal itself is actionable.
- Reporting is deterministic (cell conflicts in map order, then violations in
  `validate_merged`'s category-then-key order — Invariant #4) and always
  agrees with completion, because both run the same pure function on the same
  inputs. A repair write is reflected immediately, closing ADR-0041's
  over-reporting boundary.
- Cost: one `validate_merged` scan per `conflicts()` call (so also per
  `status`) while a merge is in progress with no outstanding cell conflicts —
  the same O(merged-graph scan) completion already pays, only mid-merge.
- ADR-0016's single-class merge outcome is deliberately unchanged: at merge
  time, cell conflicts still short-circuit validation; the mixed case's
  violations become visible exactly when the graph becomes whole.
- `PersistedConflict` is renamed `WorkspaceConflict` (its `Graph` variant now
  carries the violation); `resolve --all-ours|--all-theirs` reports any
  violations the resolution leaves behind.
