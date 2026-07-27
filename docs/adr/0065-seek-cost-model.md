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
wins only while it is selective: break-even is around **2% of the rows a
scan would visit**. Nothing in the planner knew that, so a hint fired
whenever an index existed.

Two designs were tried and rejected before this one:

1. **An absolute cap** (PR #221, 1024 candidates). Safe for ranges on a
   50k-row label, but the threshold is a *fraction* of cardinality, so one
   constant is right at exactly one graph size — 1024 is 2% only at ~50,000
   rows, and below that the cap exceeds the whole label, leaving the cliff
   intact.
2. **Tiering on the index map's prolly height.** Height is free in the
   manifest, but it changes once per fanout — roughly 10× in entries — so a
   single tier spans a 10× range of cardinalities. Calibrating to the top of
   a tier keeps the cliff for small labels; calibrating to the bottom
   needlessly declines large ones. Review measured 13–54× regressions
   surviving this design.

## Decision

**Estimate the scan's cost and spend a fixed fraction of it.**
`Snapshot::estimate_nodes` samples the nodes map — descending one path,
multiplying the fanout seen at each level and averaging a few leaves — in
`height` chunk reads, three or four in practice. An exact count is a full
walk; a stored count would be an on-disk format change. The estimate is a
**planner input only**: a wrong estimate can pick a slower plan, never a
wrong answer.

`candidate_cap(estimated_rows)` is then 2% of that, with a small floor so
point-lookup shapes still work on tiny graphs (where a scan is cheap and a
wrong choice costs little). Both seek paths — equality/composite and range —
walk the index up to `cap + 1` entries and **decline** past it, returning
`None`, the `GraphSource` contract's "cannot serve, scan instead". A result
of `cap` or fewer entries is by construction a complete walk.

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

- A declared index no longer makes a query slower: measured at parity
  (1.00×) where it was 53.7× slower, while selective cases still win
  (0.50–0.63×), and `WHERE` now benefits from indexes at all.
- The estimator is approximate. Prolly's chunking is probabilistic, so a
  skewed tree can mis-estimate; the cost is a suboptimal plan, bounded on
  both sides — decline when we could have sought (you get the scan you would
  have had) or seek slightly past break-even (bounded by the 2% fraction).
- `Snapshot::index_scan` keeps its released signature; the capped walk is a
  new `index_scan_capped`, because `Snapshot` is on STABILITY.md's frozen
  surface and this is a patch series.
- `candidate_cap` and the 2% constant are not configurable. If real
  workloads want tuning, that is a follow-up, not a reason to expose a knob
  now.
- The **UNIQUE checker is deliberately uncapped**. It is constraint
  enforcement, not optimisation: a capped walk could miss a collision and
  admit a duplicate.
