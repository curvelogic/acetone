# Phase 10 report — used in anger

*Epic `acetone-z093` · base `main @ f7e1267` (0.4.0) · this report covers PRs #238–#253*

Phase 10 is the phase where acetone acquired its first real tenant. The
phase's two working strands — paying the `7qw` quality debt and building the
open-vocabulary schema UX — converged on **first real use**: a private
external application embedding acetone as its fact store, whose consolidated
feedback then reshaped the roadmap mid-phase with Greg's direction. The
phase also completed a feature it only set out to *declare*: by its end,
**parallel edges work end-to-end through the shipped interface**, closing a
line item deferred since ADR-0030/0037 and fixing a wrong-key hazard that
had shipped silently in every release since 0.1.

Headline numbers: **TCK conformance moved from 2185/3897 (56.07%) to
2218/3897 (56.92%), still with zero failures**; the public API freeze gate
now guards **six crates** (name-list for `acetone-core`, full signatures for
the rest); the governed scan pathologies that previously ran for minutes
before their typed refusal are refused in bounded wall-clock (60 s CLI
default, ADR-0069); and a curated set of sixteen PRs each passed a fresh
adversarial review in which **every single round produced real findings** —
including two that materially corrected the design record itself.

## What shipped

**Quality tier (`7qw` P2s, PRs #238–#244).** The freeze gate's blind spot
over re-exported types closed (six per-crate `cargo-public-api` snapshots,
`bless-public-api.sh`, CI enforcement); anchor-scan memoisation
(`ScanCache`, Rc-shared into grouped projections) plus costed seek
selection (`SeekProbe`, `seek_count`) made the governed pathologies cheap
to refuse as well as bounded; the governor's byte-weighted `allocated_size`
closed a ~780× under-count; the import UNIQUE-violation path went from
quadratic to O(1) reconstruction (`claim_keys` inverse); declared
relationship property types became real (three enforcement points
mirroring ADR-0066, discriminator checked in both stored positions,
`rel-wrong-type` merge conflicts, fsck advisory coverage); and the binder
now enforces the openCypher grouping-key rules for ORDER BY and
aggregation scoping — the four families conceded at PR #33, closed with a
structural bound-tree matcher after the naive text-based approach was
proven to over-reject by 22 TCK scenarios and, in review, by thirteen
concrete paren/backtick/whitespace/`RETURN *` queries.

**Open-vocabulary tier (`yx1o`, PRs #245–#248).** Opt-in relationship-type
autodeclare (ADR-0060's rel-type half: `--autodeclare`, shell
`:autodeclare`, `Session::autodeclare`; a coining write mid-merge is
refused outright after review showed it would silently resolve a schema
conflict); `acetone schema apply` (declarative, transactional, idempotent
consumption of the `schema --json` document, with declare-time backfill
parity restored after review found the bypass, and within-entry
desired-state semantics made announced rather than silent); surrogate
`_id` minting (spec §2's ULID promise implemented — previously CREATE on a
surrogate label simply failed; the hand-rolled ULID encoding was verified
byte-for-byte against an independent reimplementation over 200k inputs,
and review caught that the undeclared `_id` type made every surrogate
point-lookup a full scan); phrase relationship-type names verified
end-to-end (spaces, unicode, punctuation, through every shipped surface);
and ADR-0071 deciding aliasing as **assert-then-identify** — where review
disproved the draft's central merge claim by running it (divergent
identifications merge *cleanly and silently duplicate*; detection is now a
stated requirement on the future `identify` design, not an inherited
property).

**Parallel edges (ADR-0073 scope addition, PRs #251–#252).** The o8r
wrong-key hazard — live since 0.1 via `import --disc`, with four shipped
symptoms including `DETACH DELETE` failing outright and a delete-plus-
create silently clobbering unrelated edges — closed by reusing the bound
edge's exact key; then Cypher `CREATE`/`MERGE` learned to resolve a
declared discriminator property into the edge key, with key-only storage,
read re-exposure under the declared name (the key winning stale-record
collisions, making divergent legacy edges editable and self-healing), and
SET/REMOVE identity guards compared against the key. Parallel edges are
now **declarable (`schema apply`), importable (`--disc`), creatable,
matchable, and curatable** — the full surface.

**Security pair (PR #253).** The two security-flagged Phase 9 review
MINORs resolved in-phase: typed id-space refusals replacing panics in the
import tracker, and a 64 MiB per-record bound making ADR-0062's
bounded-memory promise hold against pathological single records (the
reviewer independently verified the acceptance path on 73 MB inputs — no
false refusals).

**Tenant feedback → roadmap (PRs #249–#250, governing docs).** See
"Governing-document changes" below.

## Gate evidence against the ratified exit criteria

The ratified criteria live in gate bead `acetone-z093.1`; their disposition
is Greg's at the gate.

1. **Freeze gate whole-surface** — ✅ six per-crate snapshots enforced in
   CI (PR #238), proven live during the phase: three separate PRs' public
   changes were caught by the gate and re-blessed in-commit.
2. **Governed scan pathology refused in bounded wall-clock/memory through
   the shipped CLI** — ✅ measured in PR #240's evidence table: the
   pathology that ran 703 s now refuses within the 60 s CLI default
   (ADR-0069), with the deterministic caps intact; byte-weighted budgets
   close the memory half.
3. **Autodeclared rel-type CLI round-trip + schema apply + typed rel
   properties + aliasing ADR** — ✅ all four surfaces shipped and
   demonstrably reachable through the CLI (PRs #243, #245, #246, #248),
   with schema apply's byte-identical round-trip as the interchange
   evidence and ADR-0071 as the decided aliasing story.
4. **One complete cycle by the private external application** — **superseded
   by Greg's ruling** (2026-08-05, in-session): the dogfood bead
   (`z093.2`) was removed on his instruction — the application's ongoing
   construction on acetone provides the real-use exercise a staged
   demonstration cycle would duplicate. The criterion's formal
   disposition (amended, or evidenced by the integration work) is Greg's
   at the gate. What the phase *can* evidence: the tenant produced a
   consolidated asks document from real integration work; its four
   verification questions were answered from the code; its feedback
   reshaped the roadmap (ADR-0072/0073); and two of its Tier-1 needs
   (parallel edges, and the corrected workspace-ref guidance) were
   resolved within the phase.

## Governing-document changes (full adversarial review; listed per protocol)

Two governing-doc changes shipped mid-phase, both on Greg's explicit
in-session instruction, both through the full adversarial gate:

- **ADR-0072 + roadmap amendment (PR #249)**: incorporation of the
  tenant's requests — the owns-nothing daemon shape adopted as the
  server-mode target, rel-type rename/merge and two new unscheduled
  items, tenant-pull markers, and one considered decline (transferable
  workspace state). The review's blocker was the amendment's own
  "staleness fix" overstating the parallel-edges query surface — and its
  by-product re-priced `o8r` to P2 by proving the wrong-key path
  reachable, which set up the ADR-0073 scope addition.
- **ADR-0073 + roadmap + STABILITY.md (PR #250)**: Phase 11 ("Embedding
  and co-tenancy": daemon `pz0k`, multi-graph co-tenancy `j6ui`,
  rel-type rename/merge `lwv2`) defined on Greg's directions (quoted in
  the ADR; the parallel-edges-into-Phase-10 placement was the agent's
  proposal, separately confirmed by Greg in-session — recorded on
  `z093`); the `--json` changelog promise hardened into a commitment.
  The review forced honest attribution of direction versus proposal, a
  supersession annotation on ADR-0072, and the read-side clause without
  which the parallel-edges sketch specified a write-only discriminator.

Also noted for this section per the Phase 9 report's precedent: the
CLAUDE.md phase-start bullet (working agreement recorded 2026-08-01)
remains listed as awaiting Greg's in-repo review.

## ADRs taken this phase

ADR-0067 (explicit-instruction delegation, ratified pre-phase and applied
throughout), ADR-0068 (the phase itself), ADR-0069 (memoise-don't-
re-denominate + CLI wall-clock), ADR-0070 (open shape), ADR-0071
(assert-then-identify, merge premise corrected in review), ADR-0072
(tenant-feedback incorporation), ADR-0073 (Phase 11 + parallel edges);
amendments to ADR-0046/0057/0060/0062/0065 recorded in place, each dated.

## Review-findings summary

Sixteen PRs, every one through a fresh adversarial review; not one round
returned empty. Highlights that changed the product or record: the
mid-merge coinage conflict-resolution hazard (PR #245); the backfill
bypass and silent facet-stripping (PR #246); the surrogate seek regression
(PR #247); the divergent-identify merge disproof (PR #248); the
parallel-edges three-way-split correction and o8r re-pricing (PR #249);
the attribution-honesty and write-only-discriminator findings (PR #250);
the four-symptom o8r verification with a site-by-site regression matrix
(PR #251); the uneditable-divergent-edge blocker and the
deferred-type/null/REMOVE guard gaps (PR #252); the acceptance-path
verification of the import bounds (PR #253). All findings were fixed or
rebutted with citations; **no unresolved disagreements — no decision beads
were needed for review disputes**.

## Milestone security review

<!-- SECURITY-REVIEW-SECTION: inserted from the fresh reviewer's report -->

## Follow-ups crossing the boundary (ADR-0054: each with its reason)

Resolved in-phase instead of crossing: `7qw.3`, `7qw.4` (the security
pair). Crossing, with reasons recorded on `z093.3` and in each bead:

- `7qw.16` (P2, legacy declared-type trust) — parked behind the
  `format_version 2` gate (`qjzy`) **by the ratified roadmap**; explicitly
  out of Phase 10 scope.
- The merge-time discriminator-declaration gap (new, PR #252 follow-up) —
  needs design against the conflict machinery; same family as `7qw.23`
  (inert rel-type `exists()`), which stays with it.
- The `7qw` P3 quality backlog (`2ck.12`, `.11`, `.13`, `.15`, `.18`,
  `.19`, `.22`, `.24`, `.25`) — each bead states why its fix does not fit
  the boundary (design-needed, performance-not-correctness, or
  refactor-scale); none is justified by "only P3". All remain homed under
  `7qw`, which survives as the standing quality epic.
- `7qw.24`'s ORDER BY whole-match trap is called out specifically: two
  TCK scenarios are held Unsupported **only** by aggregate-outside-
  projection being unimplemented; the day that lands, they silently
  over-accept unless the recorded guard is built first.

## Open risks

- The o8r class is closed, but repositories written by 0.1–0.4.0 binaries
  may contain edges whose records carry stale values under a now-declared
  discriminator name; reads shadow them (key wins) and writes heal them,
  but the merge-time declaration gap above is the remaining route to
  creating more.
- `--format json` import still whole-parses (ADR-0062's recorded
  residual; CSV/NDJSON are bounded).
- The Phase 9 gix TOCTOU upstream filing (`63m.12`) still awaits Greg's
  go-ahead.

## Decisions queued for the boundary

1. Ratify ADR-0071/0072/0073 (and the ADR-0046/0057/0060/0062/0065
   amendments).
2. Rule on criterion 4's disposition (the `z093.2` removal).
3. Close gate `z093.1` — Greg's act (ADR-0067).
4. Open Phase 11 (`j1hq`) when ready; its exit criteria are drafted at
   opening.
5. The CLAUDE.md phase-start bullet's in-repo review (carried from the
   phase-start record).
6. `63m.12` (gix TOCTOU upstream) go-ahead, if desired.
