# Phase 7 report — installable and trustworthy

*Prepared at the Phase 7 boundary for the sprint-demo review. Phase 7 is the
epic `acetone-zlu` — the 0.2 library-API freeze gate. `main` is green at
`b443bf7` (including the milestone-security follow-up PR #136, acetone-8ln).*

Phase 7 carries the storage engine's invariant discipline **up** into the
query/runtime layer and prepares `acetone-core` to freeze as a **governed public
library API** at 0.2. The pre-0.1 review had named the shape of the problem: 0.1
shipped a rigorous storage engine below the manifest and a younger query/runtime
layer above it, and every urgent defect lived at that seam. This phase is that
seam, closed — govern the query engine, stabilise the identity and value
contracts a public API commits to, expose a library entry point, grow the merge
story up to the spec's promise, and harden the durability and format edges. The
dogfood-UX thread shipped early as 0.1.1; this epic is the hardening thread.

## What shipped

Grouped by the phase's through-line; each PR sat behind a fresh-subagent
adversarial review.

**Govern the query engine.**

- **Query resource governor** (acetone-iq6, ADR-0036). A single deterministic
  *work budget* bounds every query — wall-clock, result rows, expansion steps,
  and collection/list size — charged at every row, hop, and element-building
  site. The odometer is the catch-all backstop for a query that stays under each
  dimensional cap but does too much work overall.
- **Lazy, store-backed `IndexSeek`** (acetone-cbl.11, ADR-0040). An indexed
  lookup touches only the matching rows in the store instead of materialising a
  whole version per statement, mirroring the numeric cross-type and
  runtime-representation fixes so it never silently returns a subset.

**Stabilise the contracts the API freezes.**

- **Stable relationship identity** (acetone-rid, ADR-0037). Relationship
  identity now derives from the edge key rather than a positional `e{index}`
  that shifted as the graph changed — so a relationship keeps its identity
  across queries and edits. Shares its root cause with the duplicate-edge fix.
- **Value-domain round-trip** (acetone-vdc, ADR-0038). A typed value carrier
  threads temporal and `Bytes` values through the query adapter, so an
  untouched read→write round-trip recovers the original typed value instead of
  a stringified debug rendering. The acute symptom (temporal → string) is now
  structurally impossible.

**Expose the library surface.**

- **Library-level query entry point** (acetone-vf6, ADR-0039) and **routing the
  CLI through the façade** (acetone-ijq). A `Session` runs untrusted Cypher
  under the governor; the CLI is re-implemented as a client of the same governed
  path, so the library and the tool cannot drift in behaviour or bounds. This is
  the surface 0.2 freezes.

**Grow the merge story up.**

- **Cell-wise (per-property) merge** (acetone-clm, ADR-0035 — a Greg-gated
  decision, taken at the Phase-6 boundary demo). Two branches editing different
  properties of the same node no longer conflict, as Decision 4 of the spec
  promised.
- **Merge lifecycle** (acetone-mws, ADR-0041): abort and mid-merge recovery,
  graph-violation resolution, and post-merge re-validation.
- **Import curation via branch + merge** (acetone-6g5.11, ADR-0042):
  re-importing lands on a branch and merges, so it no longer blats manual
  annotations — reusing the cell-wise machinery.
- **Side-by-side conflicts** (acetone-s7d): `acetone.conflicts` surfaces base,
  ours and theirs together.

**Harden the seams.**

- **Query advisories** (acetone-7bn.5, ADR-0043): a schema-free `MATCH` on an
  undeclared label returns an advisory hint instead of a silent zero rows.
- **`fsck` on a damaged workspace manifest** (acetone-zhp); **detached-`HEAD`
  linked-worktree bootstrap** (acetone-cm9); **list-returning functions charge
  the collection cap** (acetone-fab).
- **Linked-worktree durability anchor** (acetone-7tf, ADR-0044). A linked
  worktree's uncommitted workspace is now foreign-`git gc`-durable, via a
  common-store anchor ref git enumerates as a gc root; `acetone gc` prunes stale
  anchors. Done autonomously to the full Gate-D-care standard (ADR + TDD +
  strongest-tier adversarial review).
- **Pin the shipped chunk profile in the goldens** (acetone-7bn.18, ADR-0045).
  The byte-exact goldens pinned only the 16 KiB test profile; the shipped 64 KiB
  profile is now additively pinned, with a cross-crate guard test. An empirical
  check drove the decision: the old dataset collapses to a single leaf at 64 KiB,
  so re-pinning (option a) would have *lost* inner-node coverage.

## Gate evidence — 0.2 exit criteria (`acetone-zlu`)

The epic's five exit criteria:

1. **`acetone-core` exposes a governed query entry point and the CLI runs
   through it** — ✅ (iq6 + vf6 + ijq).
2. **A property/fuzz regime proves no query exceeds the caps** — ✅, *after* the
   milestone-review HIGH below is fixed (acetone-8ln): the governor now bounds
   `DISTINCT` too, and the governor tests drive the pathologies to a bounded
   `ResourceExceeded`.
3. **Relationship identity + value-domain contract stable enough to freeze** —
   ✅ (rid + vdc).
4. **Cell-level merge demonstrated on the divergent-property case** — ✅ (clm).
5. **Dogfood registry has run continuously since 0.1 with no data-integrity
   incident** — Greg's operational evidence; inherently a boundary judgement.

The **0.2 freeze itself** is the Greg-gated decision this gate exists to
inform.

## Decisions taken (ADRs 0035–0045)

For ratification at the boundary. ADRs 0035–0043 record the phase's design
decisions; 0044 and 0045 are agent decisions taken autonomously to the
Gate-D-care standard and **flagged** for retrospective review.

- **0035** — cell-wise (per-property) three-way merge (Greg's call, taken).
- **0036** — the query-engine resource governor (one deterministic work budget).
- **0037** — stable relationship identity, derived from the edge key.
- **0038** — a typed value carrier for lossless value-domain round-trips.
- **0039** — a library-level Cypher query entry point (`Session`).
- **0040** — a lazy, store-backed `IndexSeek`.
- **0041** — merge lifecycle: abort, resolution, completion re-validation.
- **0042** — import vs curation, resolved via import-to-branch + merge.
- **0043** — query advisories (the schema-free undeclared-label note).
- **0044** — linked-worktree durability anchor *(agent decision, flagged)*.
- **0045** — pin the shipped 64 KiB chunk profile in the goldens *(agent
  decision, Gate-D adjacent, flagged)*.

## Review findings summary

Every code PR drew a fresh-subagent adversarial review, and the gate kept
earning its keep through the final PRs:

- **acetone-7tf** (durability): review found the anchor was **silently skipped**
  for a worktree whose id wasn't valid UTF-8 — quietly dropping durability where
  the design fails loudly elsewhere. Fixed to error the save. APPROVE + CONFIRM.
- **acetone-7bn.18** (Gate-D goldens): the reviewer **independently regenerated
  every pinned hash and framing byte** under the shipped profile (byte-exact
  match) and swept the height plateau to prove the dataset size isn't borderline.
  APPROVE.
- The governor, value-carrier and `IndexSeek` PRs each drew findings on
  unbounded paths and subset-return edges, fixed before the API surface froze
  around them.

## Milestone security review

A dedicated fresh-subagent security pass over the whole Phase 7 diff
(~10k lines, 65 files), focused on the new attack surface — the governed
`Session` API being frozen, the resource caps, the value carrier, lazy reads,
merge/persist, and the worktree-ref layout.

The review verified as sound: the governor arithmetic (saturating, no overflow),
the governed path applying to the library API and not just the CLI, the value
carrier (an already-decoded carrier, no new untrusted-decode path), lazy
`IndexSeek`'s panic-free handling of corrupt/absent data, merge/persist
robustness (pure functions over sorted maps, key-immutability enforced), and —
critically — that the worktree-anchor `<id>` is a single validated path
component that cannot inject a ref name or filesystem path.

It found **one HIGH**, now fixed:

- **The work-unit budget did not bound CPU for `DISTINCT`** (acetone-8ln). Both
  projection `DISTINCT` and `DISTINCT`-aggregates deduped with an O(n²) linear
  `equivalent` scan that charged the governor nothing, so
  `UNWIND range(0,999999) AS x RETURN DISTINCT x` burned minutes of CPU while
  the odometer read ~3M — directly contradicting exit criterion #2, the property
  being frozen at 0.2. Fixed (PR #136) by an O(n) hash-set dedup keyed on a
  canonical `Value::distinct_key()` consistent with `equivalent`, charging the
  governor per kept element; internal only, no change to the frozen public API,
  and TCK-neutral. A fresh adversarial re-review confirmed the fix.

Two LOW/INFO notes were recorded and are **not** blockers: label/index scans are
not charged per candidate (bounded by graph size, not query-amplified), and
there is no default wall-clock backstop (embedders can set one). With
acetone-8ln merged, the **gate is READY**.

## Open risks and deferred work

Filed as beads; none blocks the boundary:

- **Dogfood continuity** — exit criterion #5, Greg's operational evidence.
- **Default wall-clock backstop** and **charge label/index scans per candidate**
  — the two security LOW/INFO notes; hardening, not blockers.
- **Grouping comparison work** (acetone-bzr) — the aggregating grouping path does
  O(n log n) uncharged `global_cmp` comparisons; a log factor bounded by governed
  input rows (not a cap-evasion like the `DISTINCT` HIGH), surfaced by the
  acetone-8ln re-review. Keying grouping on `distinct_key` would make it O(n).
- **gc `has_linked_worktrees` TOCTOU** (acetone-dfh) — a pre-existing race
  surfaced by the 7tf review (not a regression; `consolidate` already cannot see
  `refs/worktree/*`).
- **Residual UX/robustness**: edit-distance length guard (acetone-7bn.20, P4),
  edge three-way values in `acetone.conflicts`, ancestry refspecs (acetone-bvq),
  error-message quality (acetone-cbl.3), TCK climb (i64::MIN `acetone-4lh`,
  pattern comprehension `acetone-cxh`, and others).

## The demo

The live demo drives the phase's own code and tooling, step by step: the
governor bounding a runaway query, cell-wise merge reconciling two branches that
edited different properties, a value reading back as its own domain rather than a
debug string, and a linked worktree's uncommitted work surviving a foreign
`git gc`. See the sprint deck (`docs/demos/phase-7-deck.html`).
