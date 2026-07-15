# ADR-0038: A typed value carrier for lossless value-domain round-trips

- Status: accepted
- Date: 2026-07-15
- Deciders: Greg (chose the opaque-carrier option over native variants at the 0.2 design fork); agent under the Phase 7 mandate
- Related: spec §4 (value domain); ADR-0029 (`acetone-2vr`/U2 — preserve deferred property types on write-back, which this supersedes); beads `acetone-vdc` (this work), `acetone-vf6` (library query entry point, which freezes the runtime `Value` API at 0.2)

## Context

The stored value domain (`acetone_model::Value`) carries `Bytes` and all four
temporals (`Date`/`Time`/`DateTime`/`Duration`) as first-class, losslessly
CBOR-encoded variants. The **runtime** value domain
(`acetone-cypher::exec::value::Value`) does **not**: it has no `Bytes` or
temporal variants, so `adapter::convert_value` renders those on read to a hex
string (`Bytes`) or a `{:?}` debug string (temporals). The reverse converters
(`persist.rs`, `adapter::model_value_of`) can only turn a runtime
`Value::String` back into `ModelValue::String`. So a read→write round-trip
through Cypher **retypes** a temporal or `Bytes` property into a string on disk.

ADR-0029 patched this for **node** write-back with a re-read heuristic: fetch
the original stored record and, if the adapter's re-rendering of a stored value
structurally equals the runtime value, keep the original typed `ModelValue`. It
explicitly left **edges** uncovered (no base record is threaded into edge
writes) and has a documented false-positive corner (deliberately `SET`ting a
deferred property to its own rendered string preserves the old typed value).

This must be fixed before `acetone-core` freezes the runtime `Value` and its
string-rendering contract as public API at the 0.2 gate (`acetone-vf6`). This is
a **purely runtime / query-adapter change** — the on-disk encoding already
carries every type losslessly, so there is **no `format_version` bump** and no
golden-vector churn.

## Decision

Give the runtime `Value` an **opaque typed carrier** variant,
`Value::Stored(acetone_model::Value)`, that holds the original stored value for
exactly those domain types the runtime does not model natively (`Bytes`,
`Date`, `Time`, `DateTime`, `Duration`). `adapter::convert_value` maps those
`ModelValue`s to `Value::Stored(..)` instead of to a string; the reverse
converters map `Value::Stored(mv) → mv`, so the round-trip is lossless for
**both node and edge** properties.

Greg chose this over adding native `Value::Bytes`/temporal variants. The
carrier is one additive variant against the frozen-at-0.2 public `Value` API,
versus five native variants that would also force real arms in all three
comparison regimes (`eq3`/`cmp3`/`global_cmp`) and ripple through ~90 `Value`
match sites. Faithful native domain types — with temporal comparison and
arithmetic — remain the post-0.2 endgame (filed as a follow-up); they are out
of scope for the v0.1 read subset, which cannot even construct a temporal
literal in Cypher.

### Behavioural equivalence (the load-bearing property)

`Value::Stored(mv)` is **behaviourally identical to `Value::String(render(mv))`
in every query semantic** — display (`format`), `type_name`, and the three
comparison regimes all treat a `Stored` value exactly as its string rendering.
It differs from a plain string in exactly one way: the write path recovers the
original typed `mv`. This guarantees **zero behaviour change** for reads and
comparisons — every existing test and the TCK stay green — while the write-back
loss is closed. A `Stored` value is produced only by the read adapter, never by
a Cypher expression, so a user-supplied string literal remains a `Value::String`
and is stored as a string (as intended).

### ADR-0029 heuristic removed

The carrier subsumes ADR-0029: an unmodified deferred property is read as
`Value::Stored(mv)` and written straight back as `mv` — no base-record re-read,
for nodes and edges alike. The re-read heuristic (`same_rendering`, the threaded
`base_record`) is removed. This also **fixes ADR-0029's false-positive corner**:
because a genuine user `SET x.p = '<string>'` yields a `Value::String` (not a
`Stored`), it is now correctly stored as a string, where the heuristic would
have wrongly resurrected the old typed value.

## Consequences

- Temporal/`Bytes` properties survive a read→write round-trip as themselves, for
  both nodes and edges; the frozen 0.2 `Value` API gains one clearly-documented
  carrier variant rather than a family of native types.
- The write path simplifies: the ADR-0029 base-record threading and
  `same_rendering` compare are deleted, and its false-positive is fixed.
- `acetone_model::Value` becomes visible in the public runtime `Value` (a
  sibling-crate coupling) — accepted as the pragmatic bridge; native variants
  would remove it but at a far larger frozen-API cost.
- The carrier is second-class: no temporal comparison or arithmetic (a `Stored`
  compares by its string rendering). Acceptable in v0.1's read subset; the
  faithful-types follow-up records the endgame.
- Rejected: native `Bytes`/temporal runtime variants now (too large a frozen-API
  commitment for the v0.1 subset); a display-time shadow value (more complex
  than a single carrier variant, same outcome).
