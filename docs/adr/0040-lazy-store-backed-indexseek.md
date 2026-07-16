# ADR-0040: A lazy, store-backed IndexSeek

- Status: accepted
- Date: 2026-07-16
- Deciders: agent under the Phase 7 mandate (mid-phase optimisation recorded by ADR; the correctness rules are load-bearing and flagged for the phase report)
- Related: ADR-0022 (in-memory IndexSeek, which this delivers the deferred follow-up to), ADR-0027 (composite index keys), ADR-0038 (`Value::Stored` carrier — the reason a string pin can match a Bytes rendering), ADR-0036 (query governor); beads `acetone-cbl.11` (this work), `acetone-6g5.3.2`

## Context

ADR-0022 shipped `IndexSeek` over an **in-memory** value map built after
materialising the whole graph version into a `GraphSnapshot`. That is a bounded
per-query win (≈5× on 44k nodes) but still pays O(whole-version) to load every
node and edge for *every* query, even one that selects a single row by a
declared index. The Phase-5 boundary explicitly deferred the real scalability
win — a **lazy, store-backed** seek that reads only the matching index entries
and fetches only those node records — to this bead.

The stored `idx/<name>` prolly map keys the **raw typed** property value
(`Bytes` as `Bytes`, a temporal as a temporal). The in-memory seek sidesteps a
type hazard by keying the *runtime rendering* instead; a store-backed seek reads
the raw keys and must confront it directly.

## Decision

Add a lazy `GraphSource` — `StoreBackedSource` in `acetone-cypher` — that reads
straight from an `acetone_graph::repo::Snapshot`:

- **`nodes_by_index`** does a bounded prolly scan of `idx/<name>` over the
  equality prefix (`index_value_prefix`) and fetches only the selected node
  records (`get_node`). New `Snapshot` primitives back it: `index_scan`,
  `out_edges`, `in_edges` — each a degree-/match-bounded prefix scan that loads
  only the leaves it needs.
- **`expand`** reads only a node's incident edges; **`node`** is a point lookup.
- **`all_nodes`/`nodes_by_labels`** still materialise (a genuine full scan is
  inherently O(version)) — but a seek/expand-anchored query never reaches them.

The read path (`Session::run_read`) uses it instead of the materialised
`GraphSnapshot`; **writes keep `GraphSnapshot`** (the overlay semantics are
already correct and writes are not the scale target), and clause-group `AT`
still materialises (a boxed `GraphSource` must own its data).

### Correctness rule 1 — lazy reads can fail mid-query

`GraphSource` is **infallible** (it was designed for a pre-materialised snapshot
that fails at *build* time). A lazy read can fail *during* execution with
nowhere to return the error. Rather than refactor the whole trait to `Result`,
`StoreBackedSource` records the first read error in an internal cell and returns
the method's empty/`None` fallback; `run_read` drains it with `take_error()`
after execution and converts it to a `QueryError`. A corrupt read therefore
**surfaces** rather than silently dropping rows. (Errors only occur on
corruption, so the wasted partial execution is irrelevant.)

### Correctness rule 2 — raw stored keys vs. rendered scan matches

A scan matches a `Bytes`/temporal property by its **string rendering** (the
`Value::Stored` carrier decays to that string under `eq3`, ADR-0038), but the
index keys the **raw** value. A raw-keyed probe with a string pin would miss
such an entry — **under-selecting**, which breaks the load-bearing rule that an
index seek returns exactly the scan's rows. So `nodes_by_index` serves a pin
only when a raw probe **cannot** miss:

- **Numeric and boolean pins — always.** An `Int`/`Float`/`Bool` pin can never
  cross-type-match a `Bytes`/temporal rendering (a hex/debug *string*) under
  `eq3`, so no raw entry it should match is keyed differently. Numeric pins
  probe **both** the `Int` and `Float` encodings (`3 = 3.0`).
- **String pins — only for a declared non-deferred scalar property.** A string
  could equal a `Bytes` value's hex rendering, so a string pin is served only
  when the indexed property's declared type is `Int`/`Float`/`String`/`Bool`
  (which rules out a stored `Bytes`/temporal). An **undeclared** or
  `Bytes`/temporal-typed property falls back to a scan (`None`).
- Plus the ADR-0022 rules: a **list** pin scans, an integer-valued **float
  ≥ 2⁵³** (non-unique i64 preimage) scans, and the index stays **null/NaN-blind**.

This relies on the contract that a declared scalar-typed property holds only
that type — enforced by `import`, honoured by the Cypher write path (which
cannot produce a `Bytes`/temporal for a string property), and outside the
supported contract for a raw graph-layer write of a mismatching type. The
consequence: an **untyped** indexed property is correct but *unaccelerated* for
string pins (a documented limitation; type the property to get the seek).

## Consequences

- A seek/expand-anchored read touches only the matching rows — the scalability
  win the secondary index exists for, no longer gated behind whole-version
  materialisation.
- Two new load-bearing correctness rules (error surfacing; the raw-vs-rendered
  fallback), both covered by tests; the index-seek == scan invariant is
  preserved on the store path.
- `GraphError` gains `InconsistentReverseEdge` (a corrupt reverse map, which
  fsck catches) so an in-edge with no forward record surfaces rather than
  silently dropping.
- Single-property indexes only (composite indexes scan-and-filter, as ADR-0022);
  writes and `AT` keep the materialised source.
- **In-edge order differs from the materialised source.** `GraphSnapshot` yields
  a node's in-edges in `edges_fwd` (global) order; the store source yields them
  in `edges_rev` order (keyed by `dst`, so by `type`/`src`). The result *set* is
  identical and openCypher leaves `MATCH` order unspecified without `ORDER BY`,
  but it is observable via `LIMIT`/`collect()` with no order key, and the read
  and write paths can now order in-edges differently. Not a correctness issue;
  add `ORDER BY` for a determined order.
- **The one behavioural divergence from the in-memory seek**: a `String` pin on
  a node that holds a `Bytes`/temporal value in a `String`-declared property
  under-selects (the entry is keyed raw, the probe is string-tagged), where the
  in-memory seek — keyed by rendering — would find it. Only reachable by a raw
  `Transaction::put_node` that violates the declared type (Cypher and `import`
  both prevent it); out of the supported contract, but recorded here.
- Rejected: refactoring `GraphSource` to be fallible (far larger blast radius
  than an error cell); re-keying the stored index by rendered value (an on-disk
  format change that would make `Bytes`/temporal indexes lossy).
- Follow-up: a fully lazy source for the write overlay and `AT`; string-pin
  acceleration for untyped properties would need either type enforcement on
  write or a rendered-value index.
