# ADR-0071: Aliasing for surrogate-keyed entities — assert with an edge, collapse with an explicit resolve

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
  `resolve` operation is the *collapsing act*.

## Decision

**Option (c): assert-then-resolve.**

1. **The assertion is an ordinary edge.** An equivalence claim lands as a
   relationship (`sameAs` or whatever vocabulary the tenant coins —
   autodeclarable under ADR-0060, typed qualifiers per ADR-0066 for
   confidence/provenance). It merges as data under the cell rules, carries
   history, and is reversible by ordinary edit. Nothing in acetone treats
   it specially: queries that want pre-resolution views traverse it
   explicitly.
2. **The collapse is an explicit `resolve` operation** — a deliberate,
   named act, never a side effect: rewrite the surrogate node into the
   target identity in one graph commit (retarget its edges, merge
   properties under an explicit conflict policy, delete the surrogate).
   This builds on the rekey machinery (relates: acetone-qdp) and shows in
   history as the binding it is.
3. **Merge behaviour follows from using ordinary machinery.** Equivalence
   edges asserted on divergent branches merge like any edges. A resolve is
   a data rewrite, so a concurrent edit of the resolved surrogate surfaces
   as ordinary conflicts (conflicts are data, Invariant #4). Two branches
   resolving the *same* surrogate into *different* targets produce
   delete/rewrite divergence — conflicts, not silent winners; the
   `resolve` design must document this shape explicitly.

**Scope of this ADR: the decision, not the implementation.** The
assertion half needs no new machinery — it works today (autodeclared edge
types with typed qualifiers, shipped this phase). The `resolve` operation
is future work, to be specced and implemented under a future epic when a
tenant's curation flow actually reaches the collapsing step; its bead
records this ADR as the governing shape.

## Consequences

- Ingestion can assert equivalence at low confidence immediately, with
  provenance, on a working branch — curation reviews the claim like any
  fact, and the claim is cheap to retract.
- The graph does *not* converge until a resolve runs: consumers that want
  resolved views before then must traverse equivalence edges themselves.
  This is deliberate — convergence is a curated act, not an inference.
- `resolve` inherits hard edges to spec later: property-merge policy,
  transitive chains (resolve A→B while B→C is asserted), and the
  divergent-resolve conflict shape named above.
- Surrogate nodes still merge as distinct across branches (spec §2);
  equivalence assertions are how a tenant reconciles them — which is the
  ingest-as-branch workflow working as intended, not a gap.
