# ADR-0068: Phase 10 — Used in anger

*Status: accepted — direction discussed and crystallised with Greg at the post-0.4.0 direction session (2026-08-01); exit criteria remain draft until Greg ratifies them at phase start · Date: 2026-08-01 · Bead: acetone-z093.3*

## Context

The 0.4.0 release closed Phase 9 and with it the roadmap's last numbered
phase. The direction discussion that followed weighed four options: a quality
pass (the reopened `acetone-7qw` epic holds real security-labelled P2s — two
order-of-magnitude resource-governor evasions, a quadratic import path — plus
the public-API freeze gate's proven blind spot over re-exported types); a
dogfooding phase (deferred at every boundary since 0.2, where the criterion
was waived for want of users); a new capability from the unscheduled list; or
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

- The phase does not start until Greg opens it (phase-gating rule,
  2026-07-24); he ratifies or amends the draft exit criteria at that moment
  and the gate bead is updated to match.
- `acetone-2ck` (Phase 9's epic) still holds its pre-Phase 9 remnants and
  stays open pending Greg's re-home-or-close ruling; nothing here moves them.
- Target version 0.5 permits breaking library changes (pre-1.0 minor), which
  the freeze-gate work may need for re-blessing; any such change remains
  deliberate and snapshot-re-blessed per ADR-0046.
