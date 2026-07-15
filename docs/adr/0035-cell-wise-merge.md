# ADR-0035: Cell-wise (per-property) three-way merge

- Status: accepted
- Date: 2026-07-15
- Deciders: Greg (merge-granularity gate, ruled at the 0.1.1→0.2 boundary); agent under the Phase 7 mandate
- Related: design-space Decision 4; spec §6 (merge); ADR-0016/`acetone-jmp` (post-merge validation semantics); beads `acetone-clm` (this decision), `acetone-6g5.11` (import curation, depends on this), `acetone-mqz` (merge property tests over indexed repos)

## Context

The design record already promises cell-wise merge. Design-space Decision 4:
"keys modified on both sides recurse to property-wise (cell-wise) merge;
same-property divergence is a conflict." Spec §6 describes merge as map-by-map
three-way.

The **implementation does not honour this**. The `nodes` map is
`node key → whole node record` (secondary labels + properties), and the prolly
three-way merge treats each map value as **opaque**. So *any* node modified on
both branches is reported as a conflict on the whole record — even when the two
branches edited **different** properties. Edge property records have the same
limitation.

This is the acute dogfood friction: the flagship scheduled-import workflow
(`import sets os_version` while a human `sets owner` on the same node) conflicts
on **every run**, when it should merge silently. It also undercuts import
curation (`acetone-6g5.11`), whose whole point is preserving human annotations
across authoritative-replace re-imports.

The two options were: **(A)** make the code honour the spec (cell-wise), or
**(B)** amend the design record to state node records merge whole. Cell-wise
three-way merge anchored on primary keys is one of the two Dolt properties the
design-space doc calls "load-bearing for acetone" (the other being history
independence).

## Decision

**Implement cell-wise merge (Option A)**, targeted at the 0.2 gate. A node or
edge modified on both branches merges property-by-property; only *same-property*
divergence is a conflict.

The conflict model therefore becomes **per-property**, not per-node. This must
be settled before 0.2 because it is part of the surface (`acetone.conflicts()`,
the `_Conflict` virtual subgraph, and the resolve UX) that the `acetone-core`
library API freezes at the 0.2 gate.

### Scope / moving parts

1. **Domain-aware value merge in `acetone-graph`.** The generic prolly merge
   stays opaque; a graph-level layer decodes base/ours/theirs records when the
   map merge flags a both-sides-modified key and merges their contents:
   - **Properties** — per-property three-way: one-sided change taken; both
     sides to the *same* value merges clean; both sides to *different* values,
     and add-vs-modify / delete-vs-modify, are property-level conflicts.
   - **Secondary labels** — set-wise three-way (add/remove per label).
   - **Key properties** are never in play (they live in the key tuple, not the
     record), so node identity is untouched.
2. **Edges too** — edge property records use the same machinery.
3. **Per-`(key, property)` conflict representation** — the `conflicts` map,
   `acetone.conflicts()`, and `_Conflict` gain per-property granularity. This is
   a **workspace-only, merge-in-progress** structure, **not** the frozen
   on-disk format, so **no `format_version` bump** — but it is a contract the
   resolve UX and the 0.2 library API expose.
4. **Per-property resolve** — a node may be partly auto-merged and partly
   conflicted; `acetone resolve` and hand-writes must resolve the conflicted
   property while **preserving** the auto-merged ones (today's whole-node
   `--all-ours/--all-theirs` is no longer sufficient on its own).
5. **Post-merge validation** unchanged in shape (dangling-edge integrity,
   uniqueness/constraint re-check) but runs over a **larger** changed set, since
   more nodes now merge successfully.

### Invariants that must be protected (by property tests, landing with the code)

- **Merge determinism (Invariant 4, spec §6):** the cell-wise merge MUST be a
  pure function over the sorted property/label set — iteration-order
  independent. This is the load-bearing correctness property.
- **Derived-map reproducibility (Invariant 5):** after a cell-wise merge,
  `edges_rev` and every declared index MUST be reindex-identical
  (`merge == reindex`), including indexes over a property that was cell-merged
  (`acetone-mqz`).
- **History independence (Invariant 1):** unaffected — identical merged record
  content still yields identical roots.

## Consequences

- The scheduled-import + human-curation workflow merges cleanly in the common
  divergent-property case; `acetone-6g5.11` becomes tractable.
- The conflict model and resolve UX change shape (per-property). This is a
  deliberate pre-freeze change so the 0.2 API does not commit to the coarser
  node-level model.
- Real work concentrates in items 3 and 4 (the per-property conflict model and
  resolve semantics), not the value merge itself.
- Rejected **Option B** (amend the design to node-level whole-record merge):
  cheaper to build but it would make the flagship import workflow conflict
  perpetually and surrender a load-bearing property; not adopted.
- The 0.2 exit criteria require this "demonstrated on the divergent-property
  case"; a property/fuzz regime covers the determinism and reindex invariants.
