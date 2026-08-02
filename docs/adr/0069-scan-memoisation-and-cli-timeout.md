# ADR-0069: Make the scan budget honest — memoise, don't re-denominate; the CLI arms a wall-clock

*Status: accepted — Phase 10 resource-honesty unit (acetone-7qw.7 + 7qw.6 + 7qw.21), exit criterion 2 · Date: 2026-08-02 · Bead: acetone-7qw.7*

## Context

The resource governor's caps are **deterministic work bounds** (ADR-0036,
ADR-0064): the same query over the same graph charges the same work and trips
the same cap everywhere, which is what makes the limits property-testable and
lets the frozen library API promise reproducible behaviour. The Phase 9
security review measured what that determinism costs on the shipped path: a
fresh (unbound) anchor in a pattern comprehension re-materialises the whole
node set from the store **once per outer row** (30–47 µs per node over
`StoreBackedSource`), so the governed pathology burns ~796 s wall and ~1.4 GB
RSS of *bounded* work before the typed `ScannedCandidates` refusal. Bounded
is not defended. Three remedies were on the table (acetone-7qw.7): re-denominate
the budget in store work, lower the default caps, or bound time directly.

## Decision

**1. Memoise the per-row store work — label scans *and* expansion probes —
keeping every charge byte-identical.** Measurement during implementation
showed the anchor materialisation was only the visible half: the dominant
cost was an `expand()` store probe **per anchor candidate per row**, which
charges no hops when the anchor has no edges — uncharged and unmemoised.
The `ScanCache` (held by the evaluation context and shared into derived
group contexts over the same graph, within the parent's borrow — so context
lifetime, backed by that crate-internal discipline rather than an
invalidation protocol, guarantees it cannot survive a mutation of the
underlying overlay) memoises
both: label-scan materialisations keyed by label set, and expansion results
keyed by (node, direction, types). The governor still charges the full
candidate count and every hop on every evaluation, hit or miss: limits trip
at exactly the same point as before, deterministically — they are simply
reached in seconds of real work rather than minutes. No budget
re-denomination, no change to `QueryLimits::default()`, no new cap.

The expansion memo is a **memory-for-time trade with a hard bound**: probes
past a deterministic retention cap (1M cached tuples per context) still run
and still charge but stop being retained, so a wide graph degrades to the
pre-memo cost profile rather than converting the governor's typed refusal
into unbounded retention. The cap governs cacheability only — never
results, never charges. In bytes rather than tuples: a cached tuple is a
cloned relationship plus neighbour, properties included, so 1M tuples is
roughly **0.2–0.5 GB at typical lab-graph record sizes and multiple GB at
pathologically fat records** — a ceiling denominated in entries, accepted
because the cap is an internal mechanism (like the governor's clock-poll
stride), not an operator promise. The label-scan memo is deliberately
*not* separately capped: its entry count is bounded by the clause's
distinct label sets (structurally small), and each entry's first fill is
charged in full against the scan budget, so its retention is already
bounded by `max_scanned_candidates` — looser than the expansion cap, but
charged, where the expansion memo's growth was the uncharged path.

**2. The CLI arms a wall-clock backstop by default; the library does not.**
`acetone query` gains `--timeout <seconds>` (default **60**, `0` disables),
mapped to the existing `QueryLimits::wall_clock`; the shell shares the same
default. The library's `QueryLimits::default()` keeps `wall_clock: None` —
a deterministic embedding must opt in to clock reads — but the CLI is an
interactive product surface where "your query was cut off at 60 s" is
strictly better than minutes of silence, whatever novel pathology produces
them. Work caps remain armed either way; the timeout is defence in depth,
not the primary bound.

## Consequences

- The measured pathology is refused quickly (criterion 2's evidence is the
  before/after CLI measurement in the PR/report), and *unknown* work-cheap,
  time-expensive pathologies are now cut off at the CLI regardless.
- Determinism properties are untouched by construction: cache hits charge
  exactly what misses charge. The property/TCK suites run unchanged.
- A legitimately long CLI query now needs `--timeout` raised or `0` — a
  product-visible behaviour change, in the CHANGELOG, flagged for Greg's
  boundary review via this ADR.
- The cache is per clause context by design: cross-clause reuse would need
  graph-identity tracking across `AT` sources for a pathology nobody has
  measured; the per-clause scope covers the per-row re-scan that was the
  actual attack, at zero invalidation complexity.
