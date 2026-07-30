# acetone-lab — asset-registry lab graph

The Phase 2 correctness and interactive-latency check (bead acetone-yzc.8):
a deterministic security-asset registry graph and the realistic registry
queries the roadmap names.

## What it models

Hosts run software; software depends on other software and is supplied by
suppliers; hosts hold certificates. Labels `Host`/`Software`/`Supplier`/
`Certificate` carry natural keys and a declared schema (so queries bind in
**Strict** mode), with secondary indexes on `Host.os`, `Certificate.not_after`
and the composite `(Host.os, Host.criticality)`. Each label's **key**
property is typed as deliberately as the rest: a declared type on a key is
what lets a primary-key point lookup be served by a seek rather than a scan
(`acetone-2ck.17`), so a registry that wants point lookups declares one.
Generation is fully
deterministic (a seeded LCG — no wall-clock or RNG, which the workspace
forbids), so a given `--scale` always yields the same graph and the same
query results.

## Run it

```bash
cargo run --release -p acetone-lab --bin lab -- /tmp/lab --scale 50000
```

`--scale` is the host count; the rest scale proportionally. `--scale
50000` → ~110k nodes / ~220k edges: the edge total matches the roadmap's
~200k, and the node total exceeds 50k because `scale` counts hosts while
software, suppliers and certificates also contribute nodes.

**The run builds two repositories.** Beside the path you give it, it also
creates `<path>-unindexed` — the identical graph without the declared
indexes, which is what makes the criterion-3 comparison a measurement of
indexing rather than of two different graphs. Budget roughly double the disk
and the build time. Neither directory is removed on exit, and both must be
absent before a re-run; the harness checks the twin path up front rather than
failing after the first build has completed.

## Interactive latency (evidence for the Phase 2 report)

At `--scale 50000` (110,200 nodes / 219,985 edges), all five registry
queries run at interactive latency once the graph is indexed:

| query                                             | rows | latency |
|---------------------------------------------------|-----:|--------:|
| certificate expiry sweep                          |  100 |  ~220 ms |
| orphaned software                                 |  500 |  ~160 ms |
| supply-chain blast radius (var-length deps)       |    1 |  ~130 ms |
| hosts by OS (indexed property)                    |    1 |   ~35 ms |
| critical hosts running a DE-supplier package      |    1 |  ~1.1 s |

(Wall-clock on the developer machine; not asserted as a hard CI threshold
— machine-dependent. The correctness of each query is asserted in
`tests/registry.rs` at a small deterministic scale.)

The heaviest query — a full-graph two-hop join over every host — is ~1 s
(at the edge of interactive; a candidate for the streaming/planner work
beyond 0.1); the point, scan, expiry and expansion queries are all well
under 250 ms.

**This drove a real fix.** The first full-scale run took 30 s, 21 s and
**147 s** on the three multi-hop queries: the executor's graph adapter
scanned the whole edge set on every expansion — O(nodes·edges) over a
MATCH. The adapter now builds id/label/adjacency indexes at construction
(`GraphSnapshot`), making node lookup, label scan and edge expansion
sub-linear. The lab graph existed to surface exactly this, and did.

## Phase 9 criterion 3 — seek versus scan (acetone-2ck.10, acetone-2ck.16)

**Which source a measurement uses is the whole story here**, so this
section says so twice.

The `Index acceleration` line the run prints measures the **in-memory**
`GraphSnapshot`, where a seek is a vector lookup and a point read costs
nothing. Its speed-up is an upper bound, not what a user sees. Earlier
releases of this document published *only* in-memory numbers (13.8x for a
range, 27.6x for a composite) without saying so — those cases are in fact
scan-shaped on a real store, and the discrepancy is what drove the cost
model in ADR-0065.

The `criterion 3` table measures the **shipped read path**: two real
repositories built from the identical deterministic generator, differing
only in whether the secondary indexes are declared, queried through
`Session` — the same path the CLI uses. Runs are interleaved, and the order
is **swapped every other iteration**, so machine drift lands on both sides
and neither side is systematically the one that runs with the other's working
set already resident; non-interleaved timing on this codebase once invented a
2x difference that vanished entirely under interleaving.

Every case checks, on every iteration, that the two sides return the same
result — so an "acceleration" that changed the answer fails rather than
flatters. Note its exact strength: each case projects `count(*)`, so this is
a **cardinality oracle** with the unindexed scan as the reference answer. It
catches a seek that under- or over-selects, which is the hazard that matters
(a seek must never return fewer rows than a scan); it would not catch a seek
returning the right *number* of wrong nodes.

That the two repositories really do hold the same graph is checked at the
roots: identical map contents yield identical prolly-tree roots regardless of
operation order, so the run compares the `nodes`, `edges_fwd` and `edges_rev`
roots of both repositories and refuses to measure if any differ. (Comparing
the builder's returned counts would prove nothing — it derives them from the
`Shape` and a seeded counter without reading the repository.)

At `--scale 50000` (110,200 nodes / 219,985 edges), best of 7:

| case                                     | indexed | unindexed | ratio |
|------------------------------------------|--------:|----------:|------:|
| IndexRange, expiring tomorrow (0.27%)    |  16.4 ms | 278.7 ms | **17.0x** |
| Composite seek, empty bucket             |   0.4 ms | 246.2 ms | **643x** |
| IndexRange, expiring this week (1.9%)    | 279.2 ms | 279.3 ms |  1.00x |
| IndexRange, expiring in 30 days (8.2%)   | 282.1 ms | 281.7 ms |  1.00x |
| Composite seek, populated bucket (2.9%)  | 247.9 ms | 249.4 ms |  1.01x |
| IndexSeek equality on `os` (20%)         | 253.1 ms | 252.0 ms |  1.00x |
| WHERE equality on `os` (20%)             | 286.4 ms | 285.8 ms |  1.00x |

Read it as two groups, then a point lookup the table cannot show.

**Selective seeks win large.** A certificate expiry sweep at 0.27% runs
17.0x faster; proving a bucket *empty* is 643x, because there are no
candidates to fetch at all. Treat the absence-proof ratio as a lower bound of
order rather than a figure: it is `scan time / fixed per-query overhead`, so
it moves with both (runs of the same case at this scale have measured 396x
and 621x, and `--scale 200` gives 3.8x). The 17.0x is the stable one.

**Unselective seeks decline to parity, and that is the designed
behaviour.** The 8.2% range and the 2.9% composite bucket are the two cases
this lab used to publish as 13.8x and 27.6x. On a real store they are
scan-shaped — measured break-even is around 1% — and before ADR-0065 they
did not merely fail to win, they lost: up to 37x for the range and 3.7x for
the composite. At this scale they now land within ±1% of the scan — the
residual is the index probe the planner pays to discover that it should
decline.

**That residual is a constant, not a proportion**, so it is only negligible
relative to a scan worth avoiding (ADR-0065). At 110,200 nodes it disappears
into the noise; on a graph small enough for the whole scan to take a
millisecond the same fixed probe dominates — measured at `--scale 200`, the
8.2% range reads 0.47x and the 20% equality cases 0.67-0.72x. Quote any of
these figures with the scale attached.

**The point lookup, which is deliberately *not* in the table.**

| case                                     | seek | full `Host` scan | ratio |
|------------------------------------------|-----:|-----------------:|------:|
| `MATCH (h:Host {hostname: 'host-49999'})` | 0.25 ms | 274.5 ms | **1104x** |

It is measured against the **label scan it replaces on the same
repository**, not against the unindexed twin, because the twin ratio is
uninformative here **by construction**: `KeySeek` is emitted from the label's
declared *key*, and the binder returns that hint before it ever consults the
index catalogue. Both twins declare `Host` with the same `hostname` key, so
both execute the identical plan and the ratio is pinned at ~1.00x whether key
seeks work perfectly or not at all. An earlier version of this document
printed that 1.00x as though the A/B had discovered a gap; the number could
not have shown either outcome. The run still prints the twin figure, as the
standing check that it stays pinned.

The scan reference *can* move, and it did. Until `acetone-2ck.17` this
lookup took **240.9 ms — 1.13x the label scan**: it was not seeking, it was
scanning all 110,200 nodes to find one node by its identity. The two figures
are the same binary over freshly built twin repositories on the same
machine, differing only in whether the key seek's guard is open.

What opened it: a string key pin used to be declined outright, because a
Bytes/temporal key value compares equal to its string *rendering* at runtime
while the stored encodings differ, so a raw probe could miss it. The equality
path had always resolved that hazard through the property's declared type;
key properties were simply never consulted for one. They are now — which is
why this lab declares a type on each label's key property, and why
`acetone-2ck.18` had to land first: until the CLI could declare a property
type at all, and until a declaration was a constraint the write path enforces
(ADR-0066) rather than an unchecked assertion, there was nothing here worth
trusting.

The run also prints the planner's own inputs — the sampled node-count
estimate, its ratio to the true count, and the resulting candidate budget —
because the estimator is sampled and measurably bimodal on skewed trees. A
timing without the estimate beside it is not comparable across rebuilds,
which is the same class of trap the original in-memory numbers fell into.

Wall-clock on the developer machine; not a CI-asserted threshold. At
`--scale 200000` the heaviest registry query (a full two-hop join) trips the
default expansion-step governor; the harness reports the trip and re-runs it
unbounded, so the latency evidence survives — any error other than a
resource cap fails the run.
