# ADR-0065: Seeks are chosen by estimated cost, and hints are candidates

- Status: accepted
- Date: 2026-07-26
- Beads: `acetone-2ck.2`, `acetone-7qw.9` (PR #224); supersedes the absolute cap in ADR-0064's sibling work (PR #221)

## Context

A declared index was not reliably beneficial. Measured at the Phase 9
boundary: declaring an index made an unselective equality query **3.7×**
slower at 2.9% selectivity, **18×** at 20%, and **53×** on a small label —
and `WHERE n.b = 3` did not use an index at all, so the form most people
write was left on the scan.

The cause is structural. A seek does one **random point read per matching
row**; the scan it replaces reads the nodes map **sequentially**. Random
reads measured ~50× costlier than sequential on loose objects, so a seek
wins only while it is selective. Nothing in the planner knew that, so a
hint fired whenever an index existed.

Break-even was measured rather than assumed, by timing the two primitives
directly — a full sequential read of the nodes map against a batch of random
point reads — across node sizes from 29 to 1036 bytes per row:

| bytes/row | rows | scan | point read | break-even |
|---:|---:|---:|---:|---:|
| 29 | 50k | 28.4 ms | 201 us | 0.28% |
| 235 | 50k | 23.5 ms | 150 us | 0.31% |
| 1036 | 50k | 56.1 ms | 185 us | 0.61% |
| 29 | 200k | 179.9 ms | 249 us | 0.36% |

End to end a query costs more on both sides, which dilutes the ratio to
nearer 1%. A third design was rejected on the strength of this table: making
the fraction **proportional to bytes per row**, on the reasoning that a scan
over fat records costs more and so buys the seek more room. It is a tidy
model and it is wrong — 36x the bytes moves break-even by about 2x, because
a point read is dominated by per-object overhead rather than size. The
`SizeEstimate`-carrying variant was written, measured, and abandoned.

Two designs were tried and rejected before this one:

1. **An absolute cap** (PR #221, 1024 candidates). Safe for ranges on a
   50k-row label, but the threshold is a *fraction* of cardinality, so one
   constant is right at exactly one graph size — below that the cap exceeds
   the whole label, leaving the cliff intact.
2. **Tiering on the index map's prolly height.** Height is free in the
   manifest, but it changes once per fanout — roughly 10× in entries — so a
   single tier spans a 10× range of cardinalities. Calibrating to the top of
   a tier keeps the cliff for small labels; calibrating to the bottom
   needlessly declines large ones. Review measured 13–54× regressions
   surviving this design.

## Decision

**Estimate the scan's cost and spend a fixed fraction of it.**
`Snapshot::estimate_nodes` samples the nodes map, estimating the **mean
fanout at each level** from a handful of nodes on that level and
multiplying: the node count at each level is the product of the mean fanouts
above it, and the entry count is the leaf count times mean leaf occupancy.
An exact count is a full walk; a stored count would be an on-disk format
change. The estimate is a **planner input only**: a wrong estimate can pick
a slower plan, never a wrong answer.

Sampling *per level* rather than following one path is what makes it
robust, and this was learned the hard way. The first implementation
descended a single path and multiplied that path's fanout at every level as
though the whole level looked like it. On a tree whose middle third carried
a large property that over-estimated by **8.4x**, which — since the caller
spends a fraction of the estimate — authorised a seek at 17% selectivity
that ran **12.5x slower** than the scan it replaced. Sampling eight nodes
spread across each level brings the worst observed error to **1.37x**, and
that counterexample to **1.04x**.

`candidate_cap(estimated_rows)` is then **0.5%** of that: half the ~1%
end-to-end break-even, because the estimator's residual error is asymmetric
in cost. Over-estimating scales the seek authorised linearly, while
under-estimating only forfeits a win, so the margin is spent on the
dangerous side. A small floor keeps point-lookup shapes working on tiny
graphs, where a scan is cheap and a wrong choice costs little.

Both seek paths — equality/composite and range — walk the index up to
`cap + 1` entries and **decline** past it, returning `None`, the
`GraphSource` contract's "cannot serve, scan instead". A result of `cap` or
fewer entries is by construction a complete walk.

**The budget is computed in two phases, so a selective seek never pays for
the estimator.** A probe returning no more than the floor is served without
sampling the nodes map at all; only one that clears the floor is worth
costing. This keeps ~25 chunk reads off the point-lookup path — the very
case an index exists for — and the estimate is memoised per source, so a
re-anchored seek samples once per query rather than once per row.

**Point reads are deferred until the whole candidate set is known to fit.**
An earlier version tested per probe, so a composite whose rows split across
probes paid the first probe's point reads before declining on the second.

**Hints become an ordered candidate list.** `BoundNodePattern::index_hints`
is a `Vec`, the binder attaches every applicable hint (equality first, since
it is usually the more selective), and the executor tries each in turn,
falling through when one declines. Without this, an equality hint that the
cost model rejected at runtime discarded a range plan the binder had skipped
attaching — measured 80–91× worse than no hint at all.

**`WHERE` equality attaches hints.** It could not before for a structural
reason: `IndexSeek`/`KeySeek` read their pinned values from the pattern's
inline property map, so a `WHERE`-sourced hint had no values. Both now carry
`values: Option<Vec<RangeBound>>`; `None` preserves the pattern-map path
exactly.

## Consequences

- **The cliff is gone, but "never slower" is not the guarantee.** Measured
  interleaved on the shipped `Session` path: the 53.7× regression is at
  1.23×, the 12.5× adversarial counterexample at 1.04×, and `WHERE` at
  1.01×, while selective cases win outright (0.16–0.24×). The residual is a
  **constant**, not a cliff: a seek that declines has still paid for its
  index probe and one cardinality sample, about 0.2 ms. On a graph small
  enough for the whole scan to take a millisecond that shows up as ~1.2×.
  The honest statement is *bounded near parity in the worst case, a large
  win in the selective case* — not *never slower*.
- The estimator is approximate, and its error is **not** symmetric in cost,
  which the first draft of this ADR got wrong. Over-estimating scales the
  authorised seek linearly; under-estimating only forfeits a win. Worst
  observed error across six tree shapes and four sizes is 1.37× high and
  0.31× low; the 0.5% fraction is set so that even the high end lands at or
  under measured break-even. Improving the estimator (sampling more
  cheaply, or weighting by subtree size) is `acetone-7qw.11`.
- **Candidates are tried in order, not costed.** A hint that *serves*
  short-circuits the list even if a later one would be far more selective —
  measured 65× off the best available plan where an equality just barely
  fits its budget while a range would match nine rows. Bounded near parity
  against a scan, since a serving equality is by definition within budget.
  Costing all candidates before materialising any is `acetone-7qw.10`.
- `Snapshot::index_scan` keeps its released signature; the capped walk is a
  new `index_scan_capped`, because `Snapshot` is on STABILITY.md's frozen
  surface and this is a patch series.
- `candidate_cap` and the 0.5% constant are not configurable. If real
  workloads want tuning, that is a follow-up, not a reason to expose a knob
  now. The constant is calibrated on **loose** objects; packing makes random
  reads cheaper, so it is conservative on a packed store.
- The **UNIQUE checker is deliberately uncapped**. It is constraint
  enforcement, not optimisation: a capped walk could miss a collision and
  admit a duplicate.
