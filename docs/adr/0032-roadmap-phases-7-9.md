# ADR-0032: Roadmap Phases 7–9

*Status: accepted — decided by ADR under the Autonomous Protocol (Greg
ratified roadmap direction A on 2026-07-10; this ADR records the extension he
asked for). Date: 2026-07-11 · Bead: acetone-7ro*

## Context

The roadmap (`docs/acetone-03-roadmap.md`) ran to Phase 6 ("Hardening towards
0.1"), which is at its boundary. Greg asked (2026-07-10, direction A ratified)
to extend it with Phases 7–9, keeping the phased-evolution cadence with hard
exit criteria and a sprint demo at each boundary. The substance is not invented
here: the pre-0.1 comprehensive adversarial review
(`docs/reports/pre-0.1-review.md`, four fresh Fable-5 reviewers) already framed
the post-0.1 work into three phases, and its findings are filed as beads. This
is a **governing-document** change, so per CLAUDE.md it carries a full
adversarial review and this ADR.

The review's central verdict shapes the sequencing: 0.1 is *two systems of
different maturity* — a rigorous, invariant-tested storage engine below the
manifest, and a younger query/runtime layer above it — and every urgent pre-tag
defect lived at that seam. The forward plan must therefore harden the upper
layer to the lower layer's discipline *before* any external API commits to it.

## Decision

Add three phases and refresh the tail of the roadmap:

1. **Phase 7 — Installable and trustworthy (L).** Carry the storage engine's
   invariant discipline up into the query/runtime layer, dogfood-UX-first: the
   query **resource governor** (`acetone-iq6`, the blocker for any library or
   embedded use); lazy store-backed `IndexSeek` (`acetone-cbl.11`); **stable
   relationship identity** (`acetone-rid`) and the **value-domain round-trip
   contract** (`acetone-vdc`) fixed *before* the library API freezes them; the
   library query entry point + CLI-through-façade (`acetone-vf6`) done after;
   **cell-level merge** (`acetone-clm`, a demo decision); merge lifecycle,
   robustness bugs, error-message quality, and dogfood UX. Exit at the **0.2
   library-API-freeze gate**.

2. **Phase 8 — Alongside code (M).** Co-tenancy of an acetone graph with an
   ordinary git repo behind one `GraphRefNamespace` concept (`acetone-gns`):
   HEAD/ref semantics, ref namespacing, and ref-scoped `migrate`/`gc` that never
   touch code history. Settle the **format-evolution policy** first — an ADR
   (`acetone-fev`) choosing read-old-write-new (`acetone-5yr`) as default and
   reserving history-rewrite `migrate` for opt-in.

3. **Phase 9 — At scale and in conformance (L).** Streaming/bounded-memory
   import, `fsck` and `merge_base` scale bounds, the openCypher TCK climb from
   41.0%, composite/range index seek, and the seeds of costed planning.

4. **Housekeeping toward reality.** Record the **distribution and release**
   channel (draft-then-publish GHA binaries → Homebrew tap; library internal
   until 0.2); mark roadmap **Gates A–D closed** and name the forward gates
   (0.2 API freeze, the merge-granularity demo call, the format-evolution ADR);
   and convert the stale Phase 1–2 **open questions** into their recorded
   resolutions (SHA-1 default per ADR-0031, `AT <ref>` with ancestry deferred to
   `acetone-bvq`, `edges_rev` not duplicated, no parallel edges per ADR-0030),
   leaving only "schema changes as their own commits" open.

Every deliverable cites the tracked bead that carries it, so the roadmap and the
issue tracker stay in step.

## Consequences

- The roadmap now covers the whole path to 0.2 and beyond with the same
  exit-criteria discipline, and the phase framing matches the review's "two
  systems" diagnosis (fix the seam before the API commits to it).
- Phase 7's exit *is* the 0.2 gate: the library API is not frozen — and the
  crates are not published — until the governor, relationship identity and the
  value-domain contract are stable. This is the concrete reason 0.1 keeps the
  library internal.
- The merge-granularity choice (cell-wise vs. amend Decision 4) is surfaced as a
  Phase-7 **demo decision** (`acetone-clm`), not silently resolved here.
- No code or on-disk format changes. The estimates are relative sizes, not
  commitments; beads remain the source of truth for what is actually ready.
- The roadmap tail is no longer stale: closed gates read as closed and settled
  questions read as settled, which is the same spec-drift hygiene ADR-0031
  applied to the specification.
