# ADR-0064: Scanning is governed on its own budget

- Status: accepted
- Date: 2026-07-26
- Bead: `acetone-2ck.15` (PR #219); follows the Phase 9 milestone security review

## Context

The executor's `Governor` charges work per produced row, per expansion hop and
per collection cell. Nothing charged for **examining anchor candidates**.

That was safe while every un-anchored scan happened once per query. Phase 9
broke the assumption: pattern comprehensions and pattern predicates are
evaluated **once per row** and may carry a fresh or anonymous leading node, so
an unbound anchor re-materialises the whole node map per row. The security
review demonstrated `UNWIND range(1,1000000) AS i RETURN size([(x)-->(y) | 1])`
hanging indefinitely on a 5,000-node graph — a one-line query, ordinary-looking,
with no resource error ever raised.

Two candidate fixes were tried and rejected in review:

1. **Charge work only.** Correct but too loose: the 100M work budget is reached
   only after tens of thousands of full scans, which is an hour of wall time on
   a small graph. Bounded is not the same as defended.
2. **Charge the expansion budget.** Tight enough, but it rejected *legitimate*
   work: a 20-row semi-join over a 100,000-node label examines 2M candidates —
   comfortably inside the work budget, but 2× the expansion budget, which is
   sized for edge traversal. The reviewer measured an ordinary
   `MATCH (a:N) WITH a LIMIT 20 MATCH (b:N) WHERE b.v = a.v` failing at 2.85 s.
   A false `ResourceExceeded` on ordinary work is worse for users than the
   pathology it guards against.

## Decision

Scanning gets **its own budget**: `QueryLimits::max_scanned_candidates`
(default 20,000,000), charged by `Governor::scan(candidates)` and surfaced as
`ResourceLimit::ScannedCandidates`. Every anchor materialisation charges it —
in `match_path` and in `pattern_exists` alike, and for seek results as well as
label scans, since a candidate superset still costs what it costs.

Two exemptions, both deliberate:

- A **pinned** anchor (a bound variable resolving to one node) is a lookup, not
  a scan, and is not charged. Charging it would let an ordinary row-by-row match
  burn the scan budget on single-node "scans".
- Nothing is exempt by *ordinality*. An earlier draft made the first scan free
  so that a single large `MATCH` could never error; the separate budget makes
  that unnecessary, and an ordinality exemption is exactly the kind of rule an
  attacker structures a query around.

`pattern_exists`'s edge traversal now also charges `hop()`, which it never did.

## Consequences

- The pathology is bounded by a budget sized for scanning: on the reviewer's
  20,000-node repository the runaway now errors rather than running for hours,
  and the bigger the graph the sooner it trips.
- Ordinary nested-loop joins keep working: 20 driving rows over 100,000 nodes
  is 2M candidates, a tenth of the default budget.
- `QueryLimits` and `ResourceLimit` both gain a public member. `QueryLimits` is
  constructed with `..Default::default()` throughout the codebase and in the
  documented usage, but a caller building it exhaustively must add the field —
  a compile error, not a silent behaviour change.
- The remaining inefficiency is unchanged: an unbound anchor still
  re-materialises per evaluation, so the budget is reached by doing genuinely
  wasteful work. Memoising anchor scans within a query (`acetone-7qw.6`) would
  make the same queries fast rather than merely bounded.
