# ADR-0027: Composite (multi-property) index keys

*Status: accepted — ratified by Greg at the Phase 6 boundary (2026-07-09) · Date: 2026-07-09 · Bead: acetone-at0*

## Context

ADR-0024 (Gate D format freeze) froze `format_version = 1` with one flagged
decision: the schema index entry was frozen as a single `{label, property}`
pair, deferring composite (multi-property) indexes to a future format bump.
The flag noted this was the one Gate D choice a reviewer might make
differently, and that widening it was *cheap now* (before the 0.1 tag, with no
data in the wild) but a *history-rewriting migrate later*.

At the boundary Greg ruled: **allow composite index keys.** Done before the
0.1 tag, this stays `format_version = 1` — the format simply *includes*
composite indexes from the start — so no migration is ever needed.

## Decision

**The declared property index is a `(label, ordered list of properties)`**; its
key is the ordered tuple of those properties' values.

### On-disk format (the pre-tag-critical change)

- **Schema.** `IndexDef` holds `properties: Vec<String>` (non-empty); the
  `schema`-map `Index` entry encodes `{label, properties: [...]}` (declaration
  order, not sorted — order is significant for a composite key).
- **Index entry.** The `idx/<name>`-map key is
  `[String(label), List(property names), List(values), node key]`, **uniformly**
  — a single-property index is the one-element-list case, *not* a bare scalar.
  This re-encodes even single-property indexes, so the golden byte pins for the
  index entry and the schema `Index` value are re-pinned deliberately (Gate-D
  discipline).
- **Maintenance & `fsck`.** `index_entry_key` gathers every indexed property's
  value in declaration order and is **composite null-blind**: a node
  contributes no entry if *any* component is absent, null or NaN (an equality
  pinning all components is never true when one is null). Transactional
  maintenance, the from-scratch `reindex`, and `fsck` share this one function,
  so they cannot diverge (Invariant #5); `reindex` reproduces identical roots.
- **CLI.** `acetone declare-index <name> --label L --property p1 --property p2 …`
  — `--property` repeats, in order.

### Query planning (deferred, no format impact)

Composite indexes are **maintained and `fsck`-verified** now, but the in-memory
**seek** (ADR-0022) still accelerates only single-property indexes; a query
that pins all of a composite index's properties falls back to a
scan-and-filter, which is **correct, just unaccelerated**. Teaching the binder
to match an all-properties-pinned composite index and the adapter to seek the
composite value tuple (with per-component numeric cross-type expansion) is a
query optimisation with no on-disk-format impact, filed as a follow-up. Doing
it after the tag costs nothing — only the *format* had to land before the
freeze ships.

## Consequences

- `acetone-model`: `IndexDef.properties`, `IndexEntry` composite key layout,
  schema `Index` codec, `index_prefix`/`index_value_prefix` take property/value
  slices; golden pins re-pinned. `acetone-graph`: composite `index_entry_key` +
  maintenance. `acetone-cypher`: `index_on` matches single-property indexes
  only (composite → scan); the adapter builds seek maps for single-property
  indexes only. `acetone-cli`: repeatable `--property`.
- ADR-0024's flagged decision is **resolved** (its status records the
  ratification). Spec §3.3 updated; composite indexes are no longer listed as a
  future format bump.
- **Follow-up (tracked):** composite index *seek acceleration* (binder +
  adapter), a query optimisation with no format impact.
