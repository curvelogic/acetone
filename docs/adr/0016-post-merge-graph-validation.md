# ADR-0016: Post-merge graph validation

*Status: accepted — ratified by Greg at the pre-0.1 boundary review (2026-07-11); originally an agent decision flagged for phase-boundary review · Date: 2026-07-06 · Bead: acetone-14c.3*

## Context

The three-way merge (acetone-14c.2) merges the `schema`, `nodes` and
`edges_fwd` maps **independently** via the prolly three-way merge, then
rebuilds the derived `edges_rev`. This is correct at the map level and
deterministic (Invariant #4), but a set of individually clean map merges can
compose into a graph that is *referentially* or *constraint* invalid:

- **Dangling edge.** `ours` adds edge `(A)-[R]->(B)`; `theirs` deletes node
  `B`. The edges map and the nodes map each merge without a key-level
  conflict, but the merged graph has an edge to a node that is gone.
- **UNIQUE collision.** The schema declares `email` UNIQUE on label `N`;
  each side adds a *different* node (distinct keys, so no cell conflict) with
  the same `email`. The merged graph has two nodes sharing a UNIQUE value.
- **Constraint tightening.** `theirs` adds `REQUIRE email` to a label while
  `ours` adds (or leaves) a node of that label without `email`.

Spec §7 requires merge to run "graph validation: dangling-edge detection …
and constraint re-validation over the changed key set", and that violations
"become structured conflicts, not errors". The previous PR (acetone-14c.2)
shipped merge with a documented caveat that a clean result was *map-clean,
not graph-validated*; this ADR removes that gap.

## Decision

Run graph validation **inside the pure `merge_manifests` core**, on the
map-clean path only, before returning `Clean`. Keeping it in the pure
function (rather than in the `Repository::merge` commit-graph wrapper) means
validation is a deterministic function of the base and merged manifests, so
it is covered by the merge property regime (acetone-14c.5) and the merge
stays reproducible (Invariant #4). A breach demotes the outcome to
`Conflicts`, never an error.

### Conflict model

The former `MergeConflict` struct (a cell-level clash: `map`, `key`,
`base`/`ours`/`theirs`) is renamed `CellConflict`, and a new unified enum
carries both conflict shapes spec §7 describes:

```
enum MergeConflict {
    Cell(CellConflict),        // key + base/ours/theirs values
    Graph(GraphViolation),     // a violation class
}
enum GraphViolation {
    DanglingEdge { edge, endpoint, role: Src|Dst },
    MissingRequired { node, property },
    UniqueViolation { label, property, value, nodes },
}
```

A single merge yields conflicts of one kind only: cell conflicts
short-circuit before the merged graph exists to validate, so cell and graph
conflicts never coexist in one outcome.

### What is validated, and scoping

Validation re-checks the two graph-level rules the map merge cannot see:

1. **Referential integrity** — every merged forward edge whose `src`/`dst`
   node is absent.
2. **Schema constraints** — existence (`REQUIRE`) and UNIQUE, matching the
   Phase-3 write-path semantics (`persist.rs::check_constraints`):
   constraints attach to the **primary label** only; a required property is
   satisfied by a key property (present by identity) or a record property; a
   UNIQUE value is taken from the node record.

Only **merge-introduced** breaches are reported (spec's "over the changed
key set"). A violation is attributed to the merge when it arises from a key
the merge changed — an added edge, a deleted endpoint, an added/modified
node — or a constraint the merge newly tightened. A breach already present
in `base` that neither side touched is left alone: the merge did not cause
it, and re-reporting it would attach unrelated history to this merge. This
is computed from the base→merged diff of the node and edge maps, so it costs
one diff per map plus a scan of the merged edges and nodes; the changed set
gates *attribution*, while detection still scans the merged graph (a
finer-grained, index-backed pass is deferred to the Phase 5 index work).

### Determinism

Violations are emitted in category order (dangling edges, then existence,
then UNIQUE), each category in key order, driven by ordered prolly scans and
`BTreeMap`/`BTreeSet` grouping. UNIQUE values are grouped by their canonical
`encode_value` bytes so equal values collide exactly.

## Consequences

- A `Merged` commit is now both map-clean and graph-valid; the acetone-14c.2
  "not graph-validated" caveat is removed from `Repository::merge` and the
  CLI help.
- `MergeConflict` is now an enum; the CLI renders both cell clashes and each
  graph-violation class. Downstream, acetone-14c.4 (the persisted `conflicts`
  map, `_Conflict` subgraph and `resolve`) builds on exactly this dual
  record shape.
- **Known boundaries (deliberate for v0.1):** validation is scoped to the
  merged graph reachable from the two branch tips; detection is a scan, not
  index-backed (Phase 5); relationship-type existence constraints and
  parallel-edge discriminators are not re-validated here (no merge path
  introduces them yet); the write path itself still admits some violating
  graphs (`persist.rs` notes the same UNIQUE same-statement gap), so merge
  validation can be *stricter* than a single write — which is the intended
  direction for a safety pass.
- Because validation lives in the pure core, the property regime
  (acetone-14c.5) can assert "clean merges never produce dangling edges" as
  a genuine invariant, and the flagship demo (acetone-14c.7, which now
  hard-depends on this bead) can show conflict-as-data for graph breaches.
