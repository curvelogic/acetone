# ADR-0009: Subagent model tiers

*Status: accepted (Greg) · Date: 2026-07-04 · Bead: acetone-xat · PR: #16*

## Context

The autonomous working protocol dispatches subagents constantly:
adversarial PR reviewers, milestone security reviewers, design helpers,
doc reviewers, exploration fan-outs, mechanical batch work. Until now
every subagent inherited the session's primary model by default. When
the primary model is a top-tier one, that is wasteful — token budgets
are shared, and most dispatched work is mechanical — but naive
economising is dangerous in the other direction: the adversarial review
gate is the protocol's backbone, and Phase 1 encodings freeze into every
future repository hash, so a missed subtle bug there is close to the
most expensive mistake this project can make.

A further constraint: the models available to a session change over
time (top-tier access comes and goes). A policy written in terms of
model names would rot within weeks.

## Decision

Match the model tier of a dispatched subagent to the **cost of an
undetected error** in its output, not to the task's prestige. The
policy is expressed in tiers relative to whatever the session offers;
model names are illustrative only.

- **Strongest available tier** — adversarial PR reviews and milestone
  security reviews. The review gate is never downgraded to save tokens,
  at least until Gate D (format freeze). After the format freezes, the
  marginal cost of a missed review finding drops (bugs become fixable
  without history rewrites) and the tier choice should be revisited.
- **Mid tier** — design/planning subagents and lighter-path doc
  reviews, escalating to the strongest tier when the work touches the
  Load-Bearing Invariants or the on-disk format.
- **Smallest tier** — exploration/search fan-outs and mechanical batch
  work, where output is cheap to verify by use (a wrong search result
  is discovered the moment it is used; a wrong review sign-off is not).

## Consequences

- The policy lives in CLAUDE.md (governing document) so fresh agents
  apply it without session memory.
- Dispatching agents must consciously pick a tier per dispatch;
  "inherit the default" remains correct only when the default is the
  strongest tier and the task is review-class.
- When top-tier access lapses (e.g. reverting to an Opus-class
  default), reviews ride the strongest tier still available — the
  gate's *relative* strength is preserved even as absolute capability
  fluctuates.
- Revisit trigger: Gate D (format freeze), recorded here so the phase
  report that closes Gate D picks it up.
