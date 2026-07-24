# ADR-0061: A required property is satisfied only by a non-null value

*Status: accepted — ruled by Greg at the Phase 9 grooming session (2026-07-24) · Date: 2026-07-24 · Bead: acetone-h7j3*

## Context

Today an existence constraint (`--require tier`, spec §2) is satisfied by
**key presence regardless of value**: a node imported from JSON as
`{"tier": null}` passes `--require tier` on the write path, in the import
checker and in merge validation alike (PR #184 review, finding 3). The
behaviour is consistent — but it arguably violates the intent of REQUIRE,
which users read as the SQL `NOT NULL` analogue: "this property has a
value".

Two neighbouring facts make the presence-suffices reading hard to defend:

- The spec already demands **key** properties be "present, non-null and
  scalar" (§2). Existence constraints reading "present, possibly null" is
  an inconsistency between the two constraint families, not a deliberate
  semantic.
- On the query surface, openCypher's own convention is that setting a
  property to `null` *removes* it — a stored null is largely an artefact of
  the import path and low-level plumbing, not something a Cypher user can
  naturally create. The presence-suffices semantics therefore mostly
  launders imported nulls past a constraint the importer asked for.

## Decision

**REQUIRE means non-null.** An existence constraint is satisfied only by a
present, non-null value — aligning existence constraints with the key-tuple
rule. Enforcement is uniform across every surface that checks constraints
today: the Cypher write path, the import checker (`--require`), and merge
re-validation. No surface may keep the old reading.

Migration posture: existing repositories may hold null-valued required
properties written under the old semantics. These surface as **ordinary
violations with the violating nodes named** — at declaration time (the
existing refuse-and-list behaviour), at import, and at merge revalidation —
with repair by ordinary writes. **No silent data rewriting**: acetone never
deletes or fills a null on the user's behalf.

## Consequences

- Spec §2 wording is updated to say explicitly that existence constraints
  require a non-null value (part of the implementing change, not a silent
  divergence).
- A validation change can refuse data that was previously accepted; the
  implementing release notes it in the CHANGELOG, and the manual's schema
  chapter documents the semantics.
- Bead acetone-h7j3 is re-scoped from "decide" to "implement": spec
  wording, uniform enforcement across the three surfaces, tests pinning
  `{"prop": null}` as a violation on each, and the declare-time violation
  listing exercised against pre-existing nulls. Homed in Phase 9
  (acetone-2ck).
