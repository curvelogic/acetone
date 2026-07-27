# acetone-lab — asset-registry lab graph

The Phase 2 correctness and interactive-latency check (bead acetone-yzc.8):
a deterministic security-asset registry graph and the realistic registry
queries the roadmap names.

## What it models

Hosts run software; software depends on other software and is supplied by
suppliers; hosts hold certificates. Labels `Host`/`Software`/`Supplier`/
`Certificate` carry natural keys and a declared schema (so queries bind in
**Strict** mode), with a secondary index on `Host.os`. Generation is fully
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
`Session` — the same path the CLI uses. Runs are interleaved (indexed,
unindexed, indexed, …) so machine drift lands on both sides; non-interleaved
timing on this codebase once invented a 2x difference that vanished entirely
under interleaving. Every case asserts the two sides return identical rows,
so an "acceleration" that changed the answer fails rather than flatters.

At `--scale 50000` (110,200 nodes / 219,985 edges), best of 7:

| case                                     | indexed | unindexed | ratio |
|------------------------------------------|--------:|----------:|------:|
| IndexRange, expiring tomorrow (0.27%)    |  17.9 ms | 285.6 ms | **15.9x** |
| Composite seek, empty bucket             |   0.7 ms | 257.3 ms | **396x** |
| IndexRange, expiring this week (1.9%)    | 290.6 ms | 295.1 ms |  1.02x |
| IndexRange, expiring in 30 days (8.2%)   | 294.8 ms | 305.8 ms |  1.04x |
| Composite seek, populated bucket (2.9%)  | 258.4 ms | 260.1 ms |  1.01x |
| IndexSeek equality on `os` (20%)         | 256.2 ms | 257.9 ms |  1.01x |
| WHERE equality on `os` (20%)             | 294.3 ms | 292.9 ms |  1.00x |
| KeySeek, one host by primary key         | 257.1 ms | 257.7 ms |  1.00x |

Read it as three groups.

**Selective seeks win large.** A certificate expiry sweep at 0.27% runs
15.9x faster; proving a bucket *empty* is 396x, because there are no
candidates to fetch at all.

**Unselective seeks decline to parity, and that is the designed
behaviour.** The 8.2% range and the 2.9% composite bucket are the two cases
this lab used to publish as 13.8x and 27.6x. On a real store they are
scan-shaped — measured break-even is around 1% — and before ADR-0065 they
did not merely fail to win, they lost: up to 37x for the range and 3.7x for
the composite. They now cost 1-4%, which is the index probe the planner
pays to discover that it should decline.

**One case is a genuine gap, not a designed decline.** `KeySeek` at 1.00x
is a *point lookup by primary key* that is scanning 110,200 nodes. String
key pins are declined outright because a Bytes/temporal key value would
compare equal to its string rendering at runtime and a raw probe would miss
it; the equality path solves that hazard with the property's declared type,
but key properties are never consulted for one. Tracked as `acetone-2ck.17`,
with `acetone-2ck.18` for the reason it is not merely an internal fix: the
CLI cannot declare property types at all, so the guard could not open for a
CLI-built graph even if keys consulted it.

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
