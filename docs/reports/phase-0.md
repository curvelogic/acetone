# Phase 0 report — Feasibility spike

*2026-07-04 · Epic acetone-28x · For Greg's review at the Phase 0 boundary.
The Gate A decision bead (acetone-28x.6) is open and is yours to close.*

## What shipped

| Bead | Deliverable | PR |
|---|---|---|
| acetone-28x.1 | Cargo workspace (six spec-§8 crates, enforced layering) + CI (fmt, clippy `-D warnings`, build/test, cargo-deny; SHA-pinned actions, read-only token) | #2 |
| acetone-28x.3 | prollytree crate evaluation — reference-only verdict, running-code evidence | #3 |
| acetone-28x.2 | Spike: prolly map over the git ODB (gear-hash CDC ~5 KiB chunks, git OIDs as the single address, gix 0.85, full ref-reachability) | #4 |
| acetone-28x.5 | History-independence property suite (five properties; mutation-tested by review) | #5 |
| acetone-28x.4 | Benchmark suite + results at 100k/1M/5M keys | #6 |
| — (process) | Autonomous working protocol, ADR-0001 | #1 |

Every PR went through the adversarial review gate; every merge had explicit
reviewer sign-off.

## Gate evidence against the roadmap's exit criteria

1. **"History independence demonstrated under randomised operation orders
   (property test)"** — met. PR #5's suite covers convergent histories,
   mutate-then-revert, empty-map stability, chunk-parameter isolation and
   cross-store determinism; 95–98% of generated cases are multi-chunk; the
   review seeded five bugs and the suite caught all three that were
   genuinely behaviour-changing (the other two were proved no-ops). No
   property ever failed against the real spike.
2. **"Update latency and repo growth acceptable at 1M keys"** — evidence in
   `docs/notes/phase0-benchmarks.md`; the judgement is yours. Updates touch
   exactly the O(log n) spine (4 chunks ≈ 20 KiB at 1M). Growth is the
   worst number: ~39 MiB retained per 1%-churn import commit (~17× changed
   payload), halved by gc, and gc itself costs 17 minutes at ~918k loose
   objects. The identified mitigation (pack-on-write with chosen delta
   bases) is **unvalidated** — weigh the number as if it might not pan out.
   My reading: acceptable for a workbench with scheduled imports, on
   condition pack-on-write validation (acetone-63m.10) runs early in
   Phase 1.
3. **"A written decision on gitoxide vs git2 and adopt-vs-build"** —
   ADR-0002 (proposed): gix ≥ 0.85, no git2 fallback needed; build
   `acetone-prolly` from scratch, prollytree as reference only.
4. **"Go/no-go on Decision 1 Option A"** — ADR-0002 recommends **go**;
   ratify by closing acetone-28x.6, or direct the Option C pivot.

## ADRs taken this phase

- ADR-0001 — autonomous working protocol (process; PR #1).
- ADR-0002 — Gate A recommendation (proposed; awaits your ratification).

## Review-gate findings summary

The gate caught real defects at every stage: round 1 on PR #1 found a
self-certifying merge loophole in the protocol itself; PR #2's reviewer
found its own cargo-deny job red on the PR plus unpinned actions; PR #4's
reviewer attacked determinism with 36 differential assertions (held) and
surfaced the apply_batch cost caveat that kept the benchmarks honest;
PR #5's reviewer mutation-tested the property suite (3/3 real bugs caught);
PR #6's reviewer reproduced the 100k results bit-exact and forced the
pack-on-write claim to be labelled unvalidated. One finding was rebutted
with evidence (phantom beads-data change in PR #4). Full trails are on the
PRs.

**Milestone security review**: GATE-READY, no blocker-class findings — see
`docs/notes/phase0-security-review.md`. Actioned immediately: branch
ruleset on `main` (PR + four required checks + no force-push — the merge
gate is now mechanism, not just policy), workflow tokens defaulted to
read-only with Actions PR-approval disabled, the spike manifest on `main`
repaired (squash-merge artefact had left it unbuildable), and the
hostile-repo hardening checklists verified onto the Phase 1 beads (an
earlier `bd comments` syntax failure had silently dropped them). The
security review also found four hostile-repo design gaps beyond the known
list — most notably that corrupt-but-well-formed data could produce
silently wrong answers rather than errors, an integrity failure — all now
normative requirements on acetone-63m.1/63m.2/63m.7. Consciously accepted:
the spike and its gix tree sit outside CI entirely (build, test and
dependency scanning) — Phase 0 only; gix comes under CI when it enters
workspace crates.

## Decisions queued for you (not made autonomously)

1. **Gate A itself** — close acetone-28x.6 to ratify ADR-0002 (or pivot).
2. **Product licence** — workspace crates carry `publish = false` and no
   `license` field; a business decision, deliberately not ADR'd.
3. **gix's MPL-2.0 transitive dep** (`uluru` via gix-pack) — needs a
   deliberate allowlist entry (file-level copyleft, generally considered
   compatible) or an alternative before Phase 1 adopts gix in workspace
   crates.
4. **Governing-document-class for executable config** (security review
   S10): `.beads/hooks/`, `.codex/` and `.claude/settings.json` execute on
   your machine when merges touch them; proposal — add these paths to the
   protocol's governing-documents review class. One-line CLAUDE.md change,
   strengthening only; applied at Phase 1 opening if you agree.

## Open risks and loose ends carried into Phase 1

- **Pack-on-write is unvalidated** (acetone-63m.10, load-bearing for the
  growth story; fallbacks named).
- **Spike hardening list** for the production rewrite is recorded on
  acetone-63m.2 (corrupt-data panics, height-0 underflow, u32 length
  truncation, scan cycle-safety, property-suite carry-overs).
- **Benchmark port** to `benches/` against real crates (acetone-63m.9).
- Housekeeping beads: spec §7 `acetone-core` naming inconsistency
  (acetone-cbl.7), CI cancel-in-progress-on-main nit (acetone-cbl.8),
  `.beads/interactions.jsonl` tracked-vs-ignored policy (acetone-cbl.9).
- Design-record staleness: the design doc's prollytree paragraph cites
  v0.3.x; the evaluation covered v0.4.0/main. Fold into the next
  design-doc revision rather than a point edit.
- MSRV (1.96) declared but not exercised in CI — accepted drift, revisit
  when there are consumers.

## Recommendation

Phase 0's purpose — retire the git-as-chunk-store risk before building on
it — is achieved with evidence rather than argument. Recommend: ratify
Gate A, rule on the licence and review-class items above, and unblock
Phase 1.
