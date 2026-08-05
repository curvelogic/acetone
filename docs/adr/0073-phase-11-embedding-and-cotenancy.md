# ADR-0073: Phase 11 — embedding and co-tenancy; parallel edges complete Phase 10

*Status: accepted — decisions 2 and 3 execute directions Greg gave
explicitly in discussion (quoted below); decision 1's placement is an
agent conclusion from that discussion which Greg saw proposed and did
not object to — flagged, like the rest, for Phase 10 boundary
ratification. Supersedes ADR-0072's placements where they differ ·
Date: 2026-08-05 · Bead: acetone-j1hq*

## Context

ADR-0072 incorporated the Phase 10 tenant's feedback conservatively:
daemon mode and multi-graph co-tenancy went to the unscheduled list,
rel-type rename/merge was "wanted before a second autodeclare tenant",
and the machine-interface gap was left to the daemon. Reviewing the
response before relaying it, Greg judged parts of it unhelpful in
precisely the dimension the tenant flagged — an embedder needs to know
*when*, and "unscheduled" answers *whether*. Three explicit directions
followed:

- **Daemon mode: "we should definitely provide this. It is our easiest
  short term path to cross language use."**
- **Machine interface meanwhile: changelog coverage is all we promise
  for now** (no schema-version field, no shape freeze).
- **Rel-type rename/merge "is going to cause us problems unless we
  prioritise, so let's bring that forward too."**

Separately, Greg asked to understand Cypher creation of parallel edges,
observing they "seem crippled without it" — correct: declarable and
import-writable but not query-reachable, with the o8r wrong-key hazard
now known to be live. The **placement** of the completion work in Phase
10 (rather than 11) is the agent's proposal from that discussion —
argued from "not delivered until reachable" and from closing the o8r
hazard rather than carrying it as an open risk — which Greg saw and did
not object to; it is direction-adjacent, not quoted direction, and the
boundary ratifies it.

## Decision

1. **Cypher-reachable parallel edges join Phase 10's scope** (two
   units): first the o8r fix — `SET`/`DELETE` reuse the *bound* edge's
   actual key instead of recomputing with a `Null` discriminator, closing
   the live wrong-key hazard; then create/merge-side discriminator
   resolution — when the edge's type declares discriminator property
   `P`, the value of `P` in the property map becomes the key's
   discriminator; `SET` of `P` is refused as an identity change; a
   declared discriminator absent from a `CREATE` map is **refused**
   (explicit identity, the node-key precedent), with the design detail
   settled in the unit's ADR if it deviates. **On read, the
   discriminator is re-exposed under its declared property name**,
   exactly as node key properties are — without this the sketch would
   deliver a write-only discriminator (`RETURN r.P` null, MERGE with
   nothing to match on, the SET guard with no value to compare); note
   the per-edge SET refusal is NEW work on the node-key precedent —
   PR #243 delivered type checks and schema-level discriminator
   stability, not a per-edge value guard. No format change: the key has
   carried the slot since ADR-0030 and import has written values since
   Phase 5. The phase's **ratified exit criteria are unchanged** — this
   is scope beyond the minimum, added while the phase is parked (the
   park is recorded on `acetone-z093`); implementation resumes when
   Greg un-parks. If these units have not landed when the boundary
   closes, ADR-0054 applies in full: named in the phase report with the
   reason, re-homed to an owning epic (Phase 11 is the natural home).
2. **Phase 11 is defined: "Embedding and co-tenancy" (size L)** —
   its target version is assigned when its exit criteria are drafted at
   phase opening (the one-phase-per-minor convention suggests 0.6, but
   that is inference, not direction) — carrying in rough order:
   - **Daemon mode** (`acetone serve`, `acetone-pz0k`): the ADR-0072
     owns-nothing shape — one process per repository on a unix socket or
     loopback; host owns auth, tenancy, repo pools, credentials and
     transport policy; acetone is handed a directory. The scope includes
     the wire protocol's latency being measured against process-per-
     command as part of the design work. This is the project's
     cross-language embedding path and the successor to `--json` as the
     machine interface.
   - **Multi-graph co-tenancy** (`acetone-j6ui`): graph selection at
     `open`, lifting the one-graph refusal, **graph-scoped
     workspace/merge-head refs** (disposable state — no format change;
     resolves the `acetone-42d` family), and **fsck namespace-scoping**
     (which repairs a known single-graph co-tenancy wart regardless).
   - **Relationship-type rename/merge** (`acetone-lwv2`): the
     autodeclare ratchet's repair, in the `migrate` family.
3. **Machine-interface promise, effective immediately**: every breaking
   change to the `--json` shape is CHANGELOG'd in the release that makes
   it — hardened from a descriptive note into a commitment in
   `STABILITY.md`. Nothing more is promised pre-daemon.

## Consequences

- The roadmap gains a Phase 11 section; daemon, co-tenancy and
  rename/merge leave the unscheduled list; the Phase 10 section notes
  the parallel-edges scope addition.
- The tenant's response can now carry real scheduling signals: parallel
  edges in Phase 10; daemon, n-graphs and rename/merge in Phase 11;
  changelog-guarded `--json` meanwhile.
- Phase 11's exit criteria are drafted at its opening, not here; this
  ADR fixes membership and intent, ratified at the Phase 10 boundary
  alongside the rest of the phase's decisions.
- ADR-0072's declined item (transferable workspace state) stays
  declined; its verification answers stand except as corrected by the
  PR #249 review (the parallel-edges three-way split).
