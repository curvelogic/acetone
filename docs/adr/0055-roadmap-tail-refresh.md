# ADR-0055: Roadmap tail refresh — forward gates closed, crates.io hold recorded

*Status: accepted — decided by ADR under the Autonomous Protocol (the crates.io
hold itself is Greg's standing decision of 2026-07-21, already recorded in
ADR-0047 point 5; this ADR records only the roadmap correction). Date:
2026-07-23 · Bead: acetone-feh*

## Context

The roadmap review at the Phase 8 boundary (2026-07-20) found the tail of
`docs/acetone-03-roadmap.md` stale in two places — the same spec-drift hygiene
ADR-0032 applied when it marked Gates A–D closed and converted the Phase 1–2
open questions into their recorded resolutions.

1. **§Decision gates** still listed the three forward gates named by ADR-0032
   as open, but all three have since closed: the **0.2 library-API freeze**
   (ADR-0046, Greg's boundary decision 2026-07-18, enforced by committed
   `cargo-public-api` snapshots); the **merge-granularity decision** —
   cell-wise, ratified at the Phase 7 / 0.2 boundary (ADR-0035); and the
   **format-evolution ADR** — read-old-write-new default, rewrite-`migrate`
   opt-in, ratified at the Phase 8 / 0.3 boundary (ADR-0048, `acetone-fev`).

2. **§Distribution** said `acetone-core` "is published bottom-up in dependency
   order only once its API stabilises at the 0.2 gate". The API did stabilise
   at that gate, but publication did not follow: Greg decided (2026-07-21,
   recorded as standing policy in ADR-0047 point 5) to **hold** crates.io
   publication until he judges the project mature enough, or an external need
   forces it. The roadmap line was therefore superseded, and left standing it
   would read as a promise the project has explicitly declined.

The consistency sweep for this change also found the same superseded claim in
two more places: the Phase 7 exit-criteria sentence ("the 0.2 gate — the point
at which … (optionally) the crates are published bottom-up") and
`docs/RELEASING.md` §"The library crates", which still said "no external API is
frozen" and framed publication as pending the 0.2 stabilisation.

## Decision

Refresh the roadmap tail (and the RELEASING.md paragraph) to record reality;
no forward-looking content changes:

1. **§Decision gates** now records the three ADR-0032 forward gates as
   **closed**, each with its deciding ADR and ratification point, and states
   that no named forward gates remain — new ones are declared by ADR as they
   arise.
2. **§Distribution** now records that the API froze at the 0.2 gate (ADR-0046)
   but the crates are **not published to crates.io** as standing policy
   (Greg, 2026-07-21; ADR-0047 point 5); bottom-up publication order is kept
   as the plan for if and when that changes, and `STABILITY.md` (not docs.rs)
   documents the frozen surface meanwhile.
3. The **Phase 7 exit-criteria** sentence gains a parenthetical noting the
   publication option was declined, pointing at §Distribution — the phase
   prose itself is left as the historical plan it is.
4. **`docs/RELEASING.md`** §"The library crates" is retitled "(held)" and its
   opening paragraph corrected: the hold, not API instability, is now what
   keeps the crates internal.

Nothing else in the roadmap is rewritten. This is a **governing-document**
change, so per CLAUDE.md it carries a full adversarial review and this ADR.

## Consequences

- The roadmap tail is no longer stale: closed gates read as closed, and the
  distribution section no longer promises a publication the project has
  explicitly put on hold. Anyone (human or agent) reading the roadmap for
  "what happens at 0.2" now finds the actual policy and its ADR.
- The crates.io decision itself is **not** made or altered here — it lives in
  ADR-0047 point 5 and is only cross-referenced. Reversing the hold is a
  Greg decision that would come with its own ADR and a RELEASING.md update.
- No code, no on-disk format, no Load-Bearing Invariants touched; no phase
  scope or exit criteria change. The estimates and phase prose remain the
  historical plan; beads remain the source of truth for what is ready.
