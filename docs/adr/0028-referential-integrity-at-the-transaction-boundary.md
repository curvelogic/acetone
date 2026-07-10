# ADR-0028: Enforce referential integrity at the transaction boundary

*Status: accepted — decided by ADR under the Autonomous Protocol (pre-0.1
hardening sprint); flagged in the pre-0.1 review report for Greg's retrospective
review · Date: 2026-07-10 · Bead: acetone-3xd*

## Context

The pre-0.1 review (docs/reports/pre-0.1-review.md) found three related defects
(U5, U6, U7) with one root cause: **referential integrity — the rule that no
edge may exist without both its endpoint nodes — was enforced only by the
Cypher write path, never at the graph transaction boundary.**

- `Transaction::put_edge` stages an edge unconditionally; `save`/`commit` never
  re-check endpoints.
- **U6**: `import` calls `put_edge` directly for edges to possibly-absent nodes,
  committing a structurally invalid graph.
- **U5**: a merge cell conflict resolved so that an endpoint node disappears
  (e.g. `theirs` deletes a node and its edge; `ours` modifies that edge;
  resolving the edge conflict to `ours` restores the edge over the now-absent
  node) reaches merge completion with a dangling edge, and neither
  `resolve`/`commit` re-validates.
- **U7**: `fsck` had no referential-integrity pass, so the resulting corruption
  was undetectable.

The merge machinery *does* validate at merge time (`validate_merged`,
ADR-0016), reporting dangling edges a merge would introduce as
`GraphViolation::DanglingEdge`. But that check runs once, on the provisional
merged manifest — not again after the user resolves conflicts, and not on the
import path at all.

## Decision

**Enforce referential integrity once, at the transaction boundary, for every
writer.** `Transaction::save_in_place` — through which *all* mutations flow,
including import, ordinary Cypher writes, and merge completion (`commit` and
`resolve_all` both call it) — validates the transaction's staged changes against
its resulting map roots, before the workspace compare-and-swap advances, and
rejects a violating transaction with a new `GraphError::DanglingEdge`.

Two ways a save can dangle an edge, both checked against the **post-transaction**
roots (so the result is independent of op order within the transaction):

1. **Edge put with an absent endpoint.** Every forward-edge key added this
   transaction must have both its `src` and `dst` node keys present in the new
   `nodes` map.
2. **Node deleted under a live edge.** Every node key deleted this transaction
   must have no surviving incident edge. A deleted node's encoded key is exactly
   the byte prefix of every edge whose *leading* endpoint is that node
   (`edge_endpoint_prefix`), so its out-edges are a bounded prefix scan of
   `edges_fwd` and its in-edges a bounded prefix scan of `edges_rev`.

The check is incremental — only the edges added and nodes deleted this
transaction are examined, and incidence is a degree-bounded prefix scan — so the
common write path pays almost nothing.

**Detection (`fsck`).** A new `check_referential_integrity` pass scans a
committed manifest's `edges_fwd` and reports any edge whose endpoint is absent
from `nodes` as an error-severity `FindingKind::DanglingEdge`. This catches
dangling edges in older or foreign-written repositories that predate this
enforcement.

### What this does not change

- **The merge-record path is untouched.** `Repository::merge` persists a
  conflicted merged manifest (which may legitimately reference an
  about-to-be-resolved node) via its own `store.put` + workspace-tree + CAS path,
  *not* `save_in_place`; and graph-level merge violations are still reported at
  merge time and leave the repository unchanged (ADR-0016). The new check governs
  only the staged-mutation path.
- **No on-disk format change.** This is pure validation; `format_version`
  is unaffected. Golden pins unchanged.
- The merge-time `validate_merged` check stays as the *first* line of defence
  (it gives structured, resolvable conflicts); the transaction-boundary check is
  the *backstop* that no completion or import can bypass.

## Consequences

- `acetone-graph`: `GraphError::DanglingEdge { rtype, role, endpoint }`;
  `check_referential_integrity` in `repo.rs` (prevent) and `fsck.rs` (detect);
  `FindingKind::DanglingEdge` (error severity). Bead acetone-3xd.
- The invariant now holds for **all** writers, not just Cypher. Import that
  references a missing endpoint fails cleanly with no commit; a merge cannot be
  completed into a dangling state.
- Property/integration tests that previously created edges with no endpoint
  nodes (they were exercising lower-level properties like edge-map symmetry) now
  seed the endpoint nodes, so they run over valid graphs — a strictly more
  faithful fixture.
- Relates to Invariant #3 (node identity / structural integrity) and ADR-0016
  (post-merge validation semantics). Follow-on merge-lifecycle work (acetone-mws
  merge abort / graph-violation resolution, acetone-jmp) builds on this backstop.
- **Pre-existing** dangling edges (from before this enforcement) are surfaced by
  `fsck` and repairable by ordinary edits; they are not auto-repaired.
