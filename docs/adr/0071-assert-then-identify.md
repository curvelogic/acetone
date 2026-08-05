# ADR-0071: Aliasing for surrogate-keyed entities — assert with an edge, collapse with an explicit identify

*Status: accepted (agent decision per the mid-phase decision rule; flagged
for Phase 10 boundary review) · Date: 2026-08-05 · Bead: acetone-yx1o.3*

## Context

Open-vocabulary ingestion (ADR-0068, the yx1o epic) creates anonymous
entities under `KEY SURROGATE` labels: a mention is recorded before anyone
knows which real thing it denotes. Sooner or later an anonymous node is
identified with a named one — or two anonymous nodes with each other — and
the graph needs a story for that moment. Three candidate shapes:

- **(a) Equivalence edge only.** Assert `-[:sameAs]->` and stop. Cheap,
  honest history, reversible — but every consumer pays forever: any query
  that wants "the entity" must coalesce across the equivalence closure,
  which is transitive, unbounded, and easy to forget. The graph never
  converges on an identity; it accumulates IOUs.
- **(b) Rekey/rewrite immediately.** Collapse the surrogate node into the
  named identity in one commit. The graph converges, queries stay simple —
  but the assertion history is destroyed, and the irreversible commitment
  is forced at ingest time, exactly when confidence is lowest. A wrong
  collapse silently corrupts identity (Invariant #3's worst failure mode).
- **(c) Both, in sequence.** The edge is the *assertion*; an explicit
  operation is the *collapsing act*.

## Decision

**Option (c): assert-then-identify.** (The collapsing act is named
`identify`, not `resolve` — `resolve` is already acetone's shipped
merge-conflict verb, advertised by the merge error message itself, and a
governing ADR must not reserve a taken name.)

1. **The assertion is an ordinary edge.** An equivalence claim lands as a
   relationship (`sameAs` or whatever vocabulary the tenant coins —
   autodeclarable under ADR-0060, typed qualifiers per ADR-0066 for
   confidence/provenance). It merges as data under the cell rules, carries
   history, and is reversible by ordinary edit. Nothing in acetone treats
   it specially: queries that want pre-resolution views traverse it
   explicitly.
2. **The collapse is an explicit `identify` operation** — a deliberate,
   named act, never a side effect: rewrite the surrogate node into the
   target identity in one graph commit (retarget its edges, merge
   properties under an explicit conflict policy, delete the surrogate).
   This builds on the rekey machinery — note `Repository::rekey` permits
   cross-label rekeys today and `identify` is inherently cross-label
   (surrogate `Mention` → named `Person`), so the same-label guard
   proposed under acetone-qdp must carry an exemption or `identify`
   cannot reuse it.
3. **Merge behaviour: partly ordinary, one shape needs design.**
   Equivalence edges asserted on divergent branches merge like any edges,
   and a concurrent edit of an identified (deleted) surrogate surfaces as
   an ordinary delete-vs-modify conflict (conflicts are data,
   Invariant #4). But two branches identifying the *same* surrogate into
   *different* targets merge **cleanly and silently** under ordinary
   machinery — both sides delete the surrogate (an identical change), and
   the two rewrites land at disjoint keys, so nothing collides: the
   result is the surrogate's facts silently duplicated under two
   identities. This is the ingest-as-branch workflow's normal case (two
   curators independently identifying one mention), so **detection is a
   stated requirement on the `identify` design, not a property inherited
   for free** — e.g. a resolution marker written at the surrogate's own
   key, so divergent identifications collide there and conflict.

**Scope of this ADR: the decision, not the implementation.** The
assertion half needs no new machinery — it works today (autodeclared edge
types with typed qualifiers, shipped this phase). The `identify` operation
is future work, to be specced and implemented under a future epic when a
tenant's curation flow actually reaches the collapsing step; its bead
records this ADR as the governing shape, including the
divergent-identification detection requirement above.

## Consequences

- Ingestion can assert equivalence at low confidence immediately, with
  provenance, on a working branch — curation reviews the claim like any
  fact, and the claim is cheap to retract.
- The graph does *not* converge until an identify runs: consumers that
  want resolved views before then must traverse equivalence edges
  themselves. This is deliberate — convergence is a curated act, not an
  inference.
- `identify` inherits hard edges to spec later: property-merge policy,
  transitive chains (identify A→B while B→C is asserted), and — as a
  requirement, not an inherited property — making divergent
  identifications of one surrogate conflict instead of silently
  duplicating.
- Surrogate nodes still merge as distinct across branches (spec §2);
  equivalence assertions are how a tenant reconciles them — which is the
  ingest-as-branch workflow working as intended, not a gap.
