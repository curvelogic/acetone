# ADR-0001: Autonomous working protocol with subagent review gates

*Status: accepted · Date: 2026-07-04 · Bead: — (process bootstrap) · PR: #1*

## Context

Greg has directed that all implementation work proceed autonomously, with his
involvement confined to phase boundaries, while requiring high-quality and
secure code at every step. That combination needs an explicit quality
mechanism that does not depend on a human in the loop, and explicit limits on
what agents may decide alone.

## Decision

Codify the Autonomous Working Protocol in CLAUDE.md: spec-first beads, TDD,
one PR per unit of work, and a mandatory merge gate consisting of adversarial
review by a fresh subagent with reviewer sign-off (not implementer
self-certification), re-review of non-trivial fixes, and a hard block on
merging under unresolved disagreement. Governing documents always take the
full review path and agents may never expand their own merge rights.
Autonomous decisions are recorded as ADRs; phase-boundary roadmap gates (A, C)
remain Greg's; mid-phase gates (B, D) are agent-decided by ADR and flagged in
phase reports. Milestone security reviews run per phase and their unresolved
blockers prevent a gate-ready recommendation.

## Consequences

Work proceeds at agent speed between boundaries; every line on `main` has
been through an independent adversarial review; Greg's attention concentrates
on phase reports and ADRs. Costs: review latency and token spend per PR, and
the residual risk that reviewer and implementer share blind spots — mitigated
by fresh-context reviewers and the phase-end security pass. Revisit if review
quality proves hollow or throughput suffers disproportionately.
