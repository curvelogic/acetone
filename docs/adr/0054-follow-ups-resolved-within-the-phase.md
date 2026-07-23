# ADR-0054: Follow-ups are resolved within the phase by default

*Status: accepted — directed by Greg at the Phase 8 / 0.3 boundary (2026-07-23) · Date: 2026-07-23 · Beads: acetone-7qw, acetone-2ck*

## Context

The autonomous protocol captures work that surfaces mid-flight — a review nit, a
"would be nice", a deferred hardening — as a follow-up bead rather than let it
derail the bead in hand. Sound in intent, but a backlog audit at the Phase 8
boundary showed the mechanism failing in two ways:

- **The pile is net-growing.** Of ~74 P3 ("follow-up" tier) beads ever created,
  only ~a third are closed; 50 were open at the audit, accumulating each phase.
  Higher-priority work clears well (P2 ~70% closed) — the drift is entirely in
  the follow-up tier, which was becoming a place where real work went to sit.
- **Important work hid there.** `acetone-xg6` — the CLI flag that made co-tenant
  init *usable* — was filed as a P3 follow-up and deferred past the boundary, so
  every mechanism-framed exit criterion went green while the phase's headline
  feature stayed unreachable from the shipped tool. It shipped only because Greg
  caught it (the process note in `docs/reports/phase-8.md`). Nothing in the
  process distinguished "genuinely optional" from "deferred-but-load-bearing",
  and nothing drained the pile.

The audit also found that the follow-ups are *not* junk — they are real bugs,
security items and correctness gaps. So the fix is not culling; it is (a) stopping
new floats and (b) giving the existing ones phase homes.

## Decision

**A follow-up is resolved within the phase that generates it by default.
Crossing a phase boundary is the exception, and an explicit, justified one.**

Concretely:

- **Raise the creation bar.** A genuinely trivial nit is fixed inline or dropped,
  not filed. A follow-up bead is for work worth a real future unit of effort.
- **Resolve in-phase.** The phase that surfaces a follow-up closes it before the
  phase closes, unless it is genuinely *out of scope* or *depends on later phase
  work*. "It's only P3" is not a reason to cross the boundary.
- **Justify every crossing.** Any follow-up that crosses the boundary is named in
  the phase report **with the reason it could not be resolved in-phase**, and is
  re-homed to an owning epic (the next phase, or a dedicated pass) — never left
  floating under the closed phase's epic.
- **A feature is not delivered until it is reachable through the shipped
  interface.** Exit criteria and bead decomposition must reflect that; the CLI/API
  surface of a feature is in-scope for the phase that ships the feature, not a
  follow-up (the `xg6` lesson).

At the boundary, closing a phase means its own follow-ups are *done or justified-
cross* — not silently deferred.

## Consequences

- **The pile self-limits.** New follow-ups are drained by the phase that creates
  them; only justified, owned items carry forward. The audit's 50-item drift
  should not recur.
- **Features can't hide in the follow-up tier.** Requiring the shipped-interface
  surface in-phase closes the `xg6` failure mode.
- **Phases run a little longer.** Draining follow-ups before closing is real work;
  the phase boundary trades a bit of latency for an honest "done".
- **The existing backlog is re-homed, not carried floating.** The 2026-07-23
  triage routed the accumulated P3s into owned epics — `acetone-7qw` (0.3.x
  quality & security pass) for the bugs/security, `acetone-2ck` (Phase 9) for
  query-engine maturity and scale — and closed the seven stale shipped-phase
  epics (Phases 1–6 + the pre-0.1 hardening sprint) that had lingered open only
  as parents of floating children.
- **Recorded in `CLAUDE.md`.** The Autonomous Working Protocol's phase-boundary
  rules gain this default so it binds future autonomous work, not just this one.
- **Revisit if:** draining in-phase proves to bloat phases beyond usefulness — in
  which case a standing, scheduled quality pass (like `acetone-7qw`) becomes the
  named home for a *bounded* set of deliberately-deferred items, still owned and
  still drained, rather than a return to floating P3s.
