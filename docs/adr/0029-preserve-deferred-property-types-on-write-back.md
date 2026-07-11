# ADR-0029: Preserve deferred property types across a read→write round-trip

*Status: accepted — decided by ADR under the Autonomous Protocol (pre-0.1
hardening sprint); ratified by Greg at the pre-0.1 boundary review (2026-07-11) · Date: 2026-07-10 · Bead: acetone-2vr*

## Context

The pre-0.1 review found (U2) that the Cypher write path **silently retypes
temporal and `Bytes` properties to strings**. The frozen value domain
(`format_version = 1`) includes `Bytes`, `Date`, `Time`, `DateTime` and
`Duration` as first-class scalar types. The runtime openCypher value
(`exec/value.rs`) does **not** — the v0.1 read subset (spec §5.1) deferred them.
So the read adapter (`exec/adapter.rs::convert_value`) renders them lossily:
`Bytes → String(hex)`, temporals → `String("{:?}")`, rather than making a node
bearing such a property unqueryable.

The bug is on the *write* side. `persist::node_key_and_record` rebuilds a
modified node's **entire** `NodeRecord` from the runtime property map, converting
each runtime value back to a `ModelValue`. A runtime `String` becomes a
`ModelValue::String`. So any write to a node that also carries a temporal/`Bytes`
property — e.g. `SET n.note = 'x'` on a node with a `DateTime` — rewrites that
untouched property as a `String`. It is reachable from the solo CLI and corrupts
the frozen value domain silently.

The complete fix is a **typed value channel**: give the runtime value faithful
`Bytes`/temporal variants so reads and writes round-trip losslessly. That ripples
through comparison, arithmetic, function evaluation and the TCK, and is tracked
as a Phase-7 item (`acetone-vdc`). It is too large for the pre-0.1 hardening
sprint.

## Decision

**Preserve an unchanged property's stored `ModelValue` verbatim on write-back.**
When persisting a *modified base node* (its runtime id decodes to its original
storage key), `persist_changes` fetches the node's stored `NodeRecord` from the
base snapshot and passes it to `node_key_and_record`. For each non-key property:

- if the property is present in the stored record **and** the adapter's rendering
  of the stored value (`adapter::convert_value(stored)`) is structurally equal to
  the runtime value, the property was **read and written back unchanged** — keep
  the stored `ModelValue`;
- otherwise convert the runtime value as before (the property was added or
  changed; conversion is lossless for the non-deferred types, so this is a no-op
  for them).

The comparison uses a small structural `same_rendering` helper over the value
shapes a stored property can take once rendered (`Null`/`Bool`/`Int`/`Float`/
`String`/`List`), because the runtime `Value` deliberately has no `PartialEq`
(openCypher equality is three-valued). Whole-property comparison also covers
deferred values nested inside a list.

This makes `adapter::convert_value` `pub(crate)` so the read rendering and the
write-back preservation check share one definition and cannot drift.

## Consequences

- A `SET`/`REMOVE`/`MERGE` on a node no longer corrupts its untouched
  temporal/`Bytes` properties. Created nodes (no base record) and genuinely
  changed properties behave exactly as before.
- **Known limitation (accepted):** if a user *deliberately* sets a deferred
  property to exactly the string the adapter would have rendered from its stored
  value (e.g. `SET n.data = 'abcd'` where `n.data` is the bytes `AB CD`), the
  original typed value is preserved instead of the string being stored. This is
  astronomically unlikely, indistinguishable to the user on read (both render to
  the same string), and resolved cleanly by the Phase-7 typed value channel
  (`acetone-vdc`), which supersedes this heuristic.
- No on-disk format change; only the write path's record assembly. Golden pins
  unaffected.
- Relates to `acetone-vdc` (the full fix) and `acetone-rid` (rel identity) — the
  general "the runtime layer treats the frozen value domain as debug strings"
  finding from the review.
- Edge (relationship) properties are **not** given this treatment: the write
  path has no per-edge base record threaded in, and edges carry temporal/`Bytes`
  properties far less commonly; the general `vdc` fix covers them. Documented as
  a known residual for `acetone-vdc`.
