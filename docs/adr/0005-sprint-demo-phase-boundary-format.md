# ADR-0005: Sprint-demo format for phase-boundary reviews

*Status: accepted (Greg's direction, 2026-07-04) · Bead: — (Greg-directed, no queueing needed) · PR: #11*

## Context

Greg defined how he wants to review each milestone: not documents-first, but
a demo-first walkthrough that ends with the documents. The protocol's
phase-boundary bullet previously specified only the written report.

## Decision

Each phase boundary opens with a **sprint demo**, agent-prepared: (1) a
several-slide presentation (Artifact) covering context, where we are, what
was done, and the problems and decisions that arose; (2) a live demo —
driving the phase's actual code, CLI and tooling in-session, step by step,
one step per turn with output quoted and discussed before proceeding
(refined at the first demo, 2026-07-04: batched steps or background runs
defeat the purpose);
(3) Greg then reviews the docs in detail at his own pace, rules on queued
decisions, and closes the gate bead.

## Consequences

The deck and demo become mandatory agent deliverables at every boundary,
alongside `docs/reports/phase-N.md`. Decks are published as Artifacts for
presentation and archived in the repo under `docs/demos/` (self-contained
HTML, date-prefixed) so the boundary record outlives the session. Roadmap exit criteria are already
demo-shaped (Phase 1's scripted end-to-end, Phase 4's flagship merge demo),
so phase demo scripts should be checked in and reviewed like any artefact —
the demo then constitutes gate evidence rather than a performance about it.
