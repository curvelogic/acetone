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

`--scale` is the host count; the rest scale proportionally
(`--scale 50000` → ~110k nodes / ~220k edges, the roadmap's target).

## Interactive latency (evidence for the Phase 2 report)

At `--scale 50000` (110,200 nodes / 219,991 edges), all five registry
queries run at interactive latency once the graph is indexed:

| query                                             | rows | latency |
|---------------------------------------------------|-----:|--------:|
| certificate expiry sweep                          |  100 |  ~210 ms |
| orphaned software                                 |    0 |  ~145 ms |
| supply-chain blast radius (var-length deps)       |    1 |  ~0.1 ms |
| hosts by OS (indexed property)                    |    1 |   ~36 ms |
| critical hosts running a DE-supplier package      |    1 |  ~1.0 s |

(Wall-clock on the developer machine; not asserted as a hard CI threshold
— machine-dependent. The correctness of each query is asserted in
`tests/registry.rs` at a small deterministic scale.)

The heaviest query — a full-graph two-hop join over every host — is ~1 s;
the point, scan, expiry and expansion queries are all well under 250 ms.

**This drove a real fix.** The first full-scale run took 30 s, 21 s and
**147 s** on the three multi-hop queries: the executor's graph adapter
scanned the whole edge set on every expansion — O(nodes·edges) over a
MATCH. The adapter now builds id/label/adjacency indexes at construction
(`GraphSnapshot`), making node lookup, label scan and edge expansion
sub-linear. The lab graph existed to surface exactly this, and did.
