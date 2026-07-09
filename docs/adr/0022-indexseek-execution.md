# ADR-0022: IndexSeek execution over the materialised query snapshot

*Status: accepted — ratified by Greg at the Phase 5 boundary (2026-07-09) · Date: 2026-07-08 · Bead: acetone-6g5.3.2*

> **Phase 5 boundary outcome (2026-07-09).** In-memory seek accepted as the
> bounded-win-first choice. The lazy, store-backed seek (the real scalability
> win — a query touching only matching rows instead of materialising the whole
> version) is **pulled into Phase 6** as `acetone-cbl.11`; it must reproduce the
> numeric cross-type and runtime-representation handling recorded here or it
> reintroduces the silent-subset bugs the review caught. `IndexRange` and
> `KeySeek` execution remain in `acetone-6g5.3.3`.

## Context

acetone-6g5.3.1 shipped the stored `idx/<name>` maps and their maintenance.
The binder already emits `BoundNodePattern.index_hint = IndexHint::IndexSeek`
for a pattern-pinned equality on a declared index (e.g. `MATCH (h:Host {os:
'linux'})`), but the executor ignored it and always anchored a leading node
pattern with a `LabelScan` + property filter (spec §5.3 names `IndexSeek/Range`
as a physical operator). This ADR records how the seek is executed, and the two
scope choices a reviewer would otherwise have to reconstruct.

## Decision

### The seek runs over an in-memory value index, not the stored prolly map

The workbench read path materialises a whole graph version into an in-memory
`GraphSnapshot` once, then executes against it (adapter module doc; the `lab`
binary loads once and times queries). Consistent with that design, the adapter
builds, at construction, a `by_index: name → (encoded value → node indices)`
map for each **declared** index — exactly as it already builds `by_label`. The
map key is the value-only memcomparable encoding (`encode_key([value])`) of the
node's **runtime** property value — the same representation `node_satisfies`
filters against and a query literal binds to — so the seek and the filter agree
on what a property is. (This is deliberately the query-space representation, not
the stored `idx/<name>` map's raw-typed key: the runtime renders `Bytes` and
temporal values to strings, so keying the raw bytes would let a string-pinned
seek silently miss them.) The two maps are independent — the stored one serves
persistence/`fsck`/`reindex`, this one serves query seeks. `IndexSeek` is then
an `O(matches)` hash lookup versus the `O(label population)` scan + filter,
null- and NaN-blind.

Because openCypher numeric equality is cross-type (`3 = 3.0`) while the encoding
is type-tagged, a numeric seek probes **both** the `Int` and `Float` buckets,
and a list pin (whose equality recurses element-wise) falls back to a scan — so
the seek is always a superset of what `node_satisfies` accepts.

Rationale: the graph is materialised regardless, so a per-query in-memory seek
is a genuine, measurable win (4.9× on `Host.os = 'debian'` over a 44k-node lab
graph: 14 ms vs 69 ms) without the larger change of a lazy, store-backed
provider that avoids materialisation. Reading the stored `idx/<name>` map
lazily — so a selective query touches only matching nodes instead of loading
the whole version — is the real scalability win and is deferred to the
"streaming provider" optimisation the adapter doc already anticipates (filed as
a follow-up bead). The stored index remains authoritative for persistence,
`fsck`, and `reindex`; the query path rebuilds the lookup structure in memory,
just as it does for labels.

### The seek is a candidate filter, not the final answer

`match_path` uses the seek result as the anchor set but still runs
`node_satisfies` over it, so the seek only needs to return a **superset** of the
matching nodes. This keeps correctness independent of the index (a multi-property
pattern `{os:'linux', criticality:3}` seeks on `os` and filters `criticality`)
and lets `GraphSource::nodes_by_index` return `None` to mean "no such index,
fall back to a label scan". `MutableGraph` (the write overlay) forwards the seek
to its base only while the overlay is empty; once any node is created, modified,
or deleted, it returns `None` so a scan preserves correctness — reads accelerate,
read-writes stay correct.

### Scope: equality `IndexSeek` only; `IndexRange` deferred

This bead wires equality `IndexSeek` end-to-end. `IndexRange` (accelerating
`WHERE n.prop > x` and ranges) needs the binder to recognise range predicates in
`WHERE` and emit a new hint variant, plus a range scan over the ordered index —
a self-contained addition filed as a follow-up bead. Equality alone satisfies
the phase exit criterion (index-accelerated query measurably faster than scan).

## Consequences

- `GraphSource` gains `fn nodes_by_index(name, value) -> Option<Vec<NodeValue>>`
  (default `None`); `GraphSnapshot` implements it, `MutableGraph` forwards it
  conditionally, and `MemoryGraph`/`EmptyGraph` keep the default. The executor's
  leading-node anchor consults it before falling back to a label scan.
- Declared indexes now cost a little more memory and build time per query
  snapshot (a value map alongside the label map) — negligible at workbench
  scale, and only for declared indexes.
- **Deferred / flagged for the Phase 5 boundary:** the in-memory-vs-lazy-store
  seek choice (a store-backed streaming provider is the scalability follow-up);
  `IndexRange`; and `KeySeek` execution (the hint exists but the leading anchor
  still label-scans for a pinned key — a separate point-lookup optimisation).
