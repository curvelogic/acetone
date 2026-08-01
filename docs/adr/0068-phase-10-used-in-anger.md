# ADR-0068: Phase 10 — Used in anger

*Status: accepted — direction discussed and crystallised with Greg at the post-0.4.0 direction session (2026-08-01); Greg opened the phase the same day, ratifying the exit criteria as drafted and ruling the criterion-4 bar at one complete cycle (recorded on gate bead acetone-z093.1) · Date: 2026-08-01 · Bead: acetone-z093.3*

## Context

The 0.4.0 release closed Phase 9 and with it the roadmap's last numbered
phase. The direction discussion that followed weighed four options: a quality
pass (`acetone-7qw`, reopened at the Phase 9 boundary — see
docs/reports/phase-9.md — holds real security-labelled P2s: the scan
budget's ~40× per-anchor under-count and a quadratic import path, alongside
`allocated_size`'s byte-payload blindness (P3) and
the public-API freeze gate's proven blind spot over re-exported types); a
dogfooding phase (the dogfood criterion has been carried as Greg's boundary
judgement at every phase since 0.1, `acetone-cbl.6` still open); a new
capability from the unscheduled list; or
a deliberate format_version 2 boundary (`acetone-qjzy`).

A concrete first tenant has now appeared: a private external application
whose data model needs an **open predicate vocabulary** — relationship types
coined on demand (exactly ADR-0060's rel-type half), anonymous entities
(`KEY SURROGATE`), qualified facts (typed relationship properties,
`acetone-7qw.12`), and an ingest-as-branch / curate-by-merge workflow that is
precisely what Phases 5, 7 and 8 built. Grooming that use case surfaced one
genuine capability gap — there is no way to *apply* the schema document that
`acetone schema --json` exports — and filed the enabler epic `acetone-yx1o`.

## Decision

**Phase 10 — Used in anger (size M, target 0.5)** combines the quality pass
and the enabler epic, converging on first real use, as written in
`docs/acetone-03-roadmap.md` §Phase 10. Structure in beads: phase epic
`acetone-z093`, containing the `acetone-7qw` and `acetone-yx1o` epics, the
dogfood unit `acetone-z093.2`, and the gate bead `acetone-z093.1` carrying
the draft exit criteria. The roadmap's unscheduled section is retitled
"Beyond the numbered phases".

Choices embedded in the scope, made here rather than deferred:

- **The phase owns `acetone-7qw`'s P2 tier**, not the whole epic's drain:
  the remainder is triaged at the boundary (resolved, or re-homed with
  justification per ADR-0054), and format-coupled residuals
  (`acetone-7qw.16`) stay parked behind the format_version 2 gate
  (`acetone-qjzy`) — in scope for that gate, not this phase.
- **New capabilities are use-pulled, not roadmap-pushed.** Views, RDF
  projection, log/blame and the rest of the unscheduled list stay unscheduled
  until real use pulls them; the format_version 2 boundary waits until the
  `acetone-qjzy` pile justifies its cost.
- **The dogfooding application is deliberately unnamed** in the public record
  (repo, beads, reports); gate evidence will be reported generically. The
  precedent is `acetone-cbl.6`'s "private GitHub remote" framing.
- **Exit criteria are outcome-framed**, not mechanism-framed — the Phase 8
  lesson (a feature is not delivered until reachable through the shipped
  interface) applied prospectively: the autodeclare criterion is a CLI
  round-trip, not a library capability.

## Consequences

- The phase does not start until Greg opens it (the phase-start rule in
  CLAUDE.md §Autonomous Working Protocol — a working agreement of
  2026-07-24, recorded in the repo by this PR); he ratifies or amends the
  draft exit criteria at that moment and the gate bead is updated to match.
- `acetone-2ck` (Phase 9's epic) still holds its pre-Phase 9 remnants and
  stays open pending Greg's re-home-or-close ruling; nothing here moves them.
- Target version 0.5 permits breaking library changes (pre-1.0 minor), which
  the freeze-gate work may need for re-blessing; any such change remains
  deliberate and snapshot-re-blessed per ADR-0046.

Related: ADR-0032 and ADR-0055 (the roadmap-extension precedent this
follows); ADR-0054 (in-phase follow-up resolution and the shipped-interface
corollary the exit criteria apply prospectively); ADR-0060 (autodeclare);
ADR-0046 (API freeze).
