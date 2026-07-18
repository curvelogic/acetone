# ADR-0037: Stable relationship identity

- Status: accepted — ratified by Greg at the Phase 7 / 0.2 boundary review (2026-07-18)
- Date: 2026-07-15
- Deciders: agent under the Phase 7 mandate (mid-phase design decision — recorded for retrospective review; no format-freeze gate applies, see below)
- Related: spec §2 (relationship identity), §3.3 (edge maps); design-space Decision on edge identity (`docs/acetone-01-design-space.md`); ADR-0008 (edge-key discriminator layout), ADR-0030 (reject duplicate `CREATE` edge — the interim parallel-edge measure); beads `acetone-rid` (this work), `acetone-8yn`/`acetone-o8r` (query-reachable discriminators, out of scope here), `acetone-vf6` (library query entry point that freezes this identity)

## Context

The query layer improvises relationship identity: `adapter.rs::rel_value`
sets a relationship's `EntityId` to `format!("e{index}")`, the **positional
row number** of the edge in the snapshot's build order. This is asymmetric
with nodes, which already receive a stable, storage-derived identity
(`node_entity_id` = the node's memcomparable key bytes).

`e{index}` is stable only *within a single snapshot build*. It shifts whenever
the edge set changes — inserting or deleting any earlier edge renumbers every
later edge — so it is **not stable across versions/snapshots** and does not
round-trip back to the edge's storage key. Consequences:

- **`id()` / DELETE / SET targeting** a specific relationship across snapshots
  is unsound — the handle renumbers.
- **Diff/merge edge correlation** across versions cannot use it.
- **Variable-length relationship uniqueness** ("a relationship is traversed at
  most once per MATCH") works today only because ids happen to be stable within
  the one build — it rests on an accident, not a guarantee.

This must be fixed **before `acetone-core` freezes relationship identity as
public API at the 0.2 gate** (`acetone-vf6`).

Crucially, the on-disk edge key is **already** `(src, type, dst, disc)` at
`format_version 1` (`graph_keys.rs::EdgeKey`; spec §2/§3.3): the discriminator
slot has existed since the first format. So a stable identity is **derivable
from data already on disk** — this is a query-layer implementation gap against
an already-specified, already-stored identity model, directly parallel to the
cell-wise-merge situation (ADR-0035). Spec §2 mandates exactly this identity.

## Decision

Derive a relationship's runtime `EntityId` from its **edge key's
memcomparable forward-key bytes** (`EdgeKey::encode_fwd`), mirroring how nodes
already work — replacing the positional `e{index}`. Because the memcomparable
encoding is injective, distinct edges get distinct ids; because it is a pure
function of the on-disk key, the id is stable across snapshots and round-trips
back to the `EdgeKey`.

**No `format_version` bump, no migration** — nothing on disk changes; only the
query layer's derivation of an in-memory identifier. This is why the decision
sits as a mid-phase ADR rather than a format-freeze (Gate D) decision.

### Scope

This ADR covers the **stable-id derivation only**. Created edges in the write
overlay keep their existing counter-based `fresh_id()` (an `OVERLAY_ID_TAG`-
prefixed identity) — exactly as created *nodes* already do. The two id-spaces
cannot collide: `encode_fwd`'s first byte is a key type tag in `0x01..=0x0c`,
which `OVERLAY_ID_TAG` is deliberately chosen to avoid (the same disjointness
guarantee node identity already relies on).

**Out of scope:** query-reachable discriminators for *genuine parallel edges*
(the Cypher write path hardcodes `disc = Null`, and `RelTypeDef.discriminator`
is unplumbed). That is a separable write-path + schema change — also needing no
format bump, since the slot exists — and stays with `acetone-8yn`/`acetone-o8r`
(whose ADR-0030 interim measure is to reject a duplicate `CREATE`). Real
parallel edges are not enabled by this ADR.

### Invariants

Query-layer only: no key/value encoding, prolly-tree root, or merge changes, so
**Load-Bearing Invariants 1–5 are untouched**. The injective memcomparable
edge-key encoding is what makes the new identity sound (distinct edges ⇒
distinct ids), and the id now genuinely round-trips to the storage key.

## Consequences

- Relationship identity becomes stable across snapshots and round-trippable to
  the `EdgeKey`, so `id()`, cross-version DELETE/SET targeting, and diff/merge
  edge correlation are now sound to build on, and var-length uniqueness rests on
  a guarantee rather than an accident. `acetone-core` can freeze it at 0.2.
- Symmetry with node identity: base entities key-derived, created entities
  overlay-counter — one model for both.
- Rejected: adding a surrogate rel-id column to the edge key/record — that would
  change the on-disk shape and trip the format freeze, for no benefit the
  existing key can't already provide.
- The positional `e{index}` (and its `index` parameter) is removed.
