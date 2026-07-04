# Phase 0 benchmarks: prolly map over the git ODB (acetone-28x.4)

Benchmark results for the Phase 0 feasibility spike (`spikes/prolly-git-spike`,
PR #4), against the roadmap's representative asset-registry envelope. The
harness is `spikes/prolly-git-spike/src/bin/bench.rs`; scenarios, scales and
seeds are deterministic, so runs are exactly reproducible.

## Machine and environment

| | |
|---|---|
| Chip | Apple M3 Max (14 cores) |
| RAM | 96 GiB |
| OS | macOS 26.5.1 (arm64) |
| Storage | Apple internal NVMe (APFS) |
| Rust | stable 1.96, `--release` (with debug info) |
| git CLI | 2.48.1 (used for `gc`, `count-objects`, `du` sizing) |
| Harness commit | `6ef3993` on branch `acetone-28x.4/phase0-benchmarks` |

Repos live on local APFS under a temp directory. All numbers are single-run
(this is feasibility evidence, not a statistically rigorous benchmark);
point-read/scan/update figures are distributions over 100–1,000 samples
within the run.

Whole-run wall time and peak RSS (via `/usr/bin/time -l`): 100k — 211 s,
0.6 GiB; 1M — 2,937 s (49 min, dominated by the two gc passes), 4.8 GiB;
5M — 121 s (growth scenario not run at 5M, see scenario 6), 2.9 GiB.
**The 5M scale was entirely feasible on this machine**: `bulk_load`
materialises and sorts the full entry vector, but at ~230-byte values that
is ~3 GiB peak — no failure mode was hit.

## Caveats — read these before the numbers

Carried from the spike review (PR #4) and from how the harness works:

1. **Update latency is not architecture-representative.** The spike's
   `apply_batch` loads *all* internal nodes per batch (`load_levels`), so
   single-key update latency scales with tree size in a way a real
   implementation would not (it would load only the root→leaf path — 3–4
   chunk reads). **Chunk-write counts are the meaningful write-amplification
   number; latencies for `apply_batch` are spike-only.**
2. **`chunks_written()` counts the manifest blob**: +1 per commit. Chunk
   counts quoted for `apply_batch` alone exclude commits; per-commit counts
   in the growth scenario include the manifest.
3. `commit_root` walks every internal node of the version to build the
   reachability tree (`chunks/<hh>/<hex>`), and rewrites any changed shard
   trees. That cost is part of the Option A story and is measured (it is the
   `commit_per_commit` line), but its latency, like apply latency, includes
   spike-grade inefficiency.
4. All writes are loose objects (gix surfaces no streaming pack writer at
   the `Repository` level), hence the loose/packed distinction throughout.
5. Values are JSON-ish synthetic asset records (~150–300 bytes, mean ~230),
   structured text with repeated field names, so they compress roughly like
   real registry records rather than like random bytes.
6. Single machine, warm page cache; point reads are measured after a 50-read
   warm-up. Cold-cache behaviour is not measured.

## Scenario 1 — bulk load

| Scale | Load time | Throughput | Height | Chunks written | Repo size (loose) |
|---|---|---|---|---|---|
| 100k | 0.67 s | 148 k keys/s | 3 | 2,250 | 18.4 MiB |
| 1M | 6.0 s | 167 k keys/s | 4 | 22,903 | 176.4 MiB |
| 5M | 30.4 s | 164 k keys/s | 5 | 114,338 | 877.7 MiB |

## Scenario 2 — point reads (loose objects)

Random sample of 1,000 existing keys, `Store::get` (root→leaf descent each
time, no caching above the ODB).

| Scale | p50 | p99 | mean | max |
|---|---|---|---|---|
| 100k | 104 µs | 158 µs | 107 µs | 215 µs |
| 1M | 123 µs | 169 µs | 124 µs | 213 µs |
| 5M | 170 µs | 214 µs | 170 µs | 245 µs |

## Scenario 3 — range scans

Full forward scan plus 100 random 1,000-key windows.

| Scale | Full scan | Throughput | Window p50 | Window p99 |
|---|---|---|---|---|
| 100k | 0.12 s | 844 k keys/s | 1.19 ms | 1.42 ms |
| 1M | 1.08 s | 927 k keys/s | 1.15 ms | 1.41 ms |
| 5M | 5.54 s | 902 k keys/s | 1.26 ms | 1.46 ms |

## Scenario 4 — single-key update

100 successive single-key updates (`apply_batch` with one put each). Chunk
counts exclude commits (caveat 2); latency is spike-only (caveat 1).

| Scale | Chunks written mean | p50 | max | Apply latency p50 | p99 |
|---|---|---|---|---|---|
| 100k | 3.2 | 3 | 5 | 1.52 ms | 2.33 ms |
| 1M | 4.1 | 4 | 8 | 7.04 ms | 10.7 ms |
| 5M | 5.2 | 5 | 10 | 32.8 ms | 40.4 ms |

Write amplification is the headline: a single-key update rewrites one leaf
chunk plus the root→leaf spine — mean 3.2/4.1/5.2 chunks at heights 3/4/5,
i.e. ~O(log n) chunks (~15–25 KiB) per update, exactly as the architecture
predicts. Committing the version adds the manifest blob (+1) and the
reachability-tree walk: 31 ms at 100k, 50 ms at 1M, 115 ms at 5M per
commit. The latency growth across scales (1.5 ms → 7 ms → 33 ms) is the
caveat-1 spike artefact (`load_levels` reads every internal node: roughly
20 at 100k, 185 at 1M, 915 at 5M): a real implementation would read 3–5 chunks
instead, putting single-key update latency in the same sub-millisecond
band as point reads.

## Scenario 5 — diff between adjacent versions (1% churn)

The spike has no structural diff, so the harness implements the minimal
OID-comparison tree walk (two roots, descend only where child OIDs differ,
key-merge only mismatched leaves). Churn batch: 1% of keys — 90% value
updates, 5% inserts, 5% deletes. The walk's output is verified in-run
against the applied op set at every scale, and against a scan-based diff at
100k.

| Scale | Changes found | Diff time | Chunks read | Verified |
|---|---|---|---|---|
| 100k | 997 | 78 ms | 1,826 | ops + scan |
| 1M | 9,955 | 874 ms | 18,020 | ops |
| 5M | 49,786 | 4.55 s | 90,156 | ops |

Cost tracks the size of the change, not the size of the map: ~1.8–2 chunk
reads and ~90 µs per changed key at every scale (the two sides' mismatched
leaves both get read). This is direct evidence that O(diff) walks work on
the structure — unchanged subtrees are skipped by OID equality, which holds
between adjacent versions because `apply_batch` reuses unchanged chunks
bit-identically.

## Scenario 6 — repo growth: 100 import commits at 1% churn

100 successive commits, each applying value updates to a random 1% of keys
then committing (manifest + full reachability tree). Run on the 1M-key graph
per the roadmap; also run at 100k for a scaling point; not run at 5M
(the roadmap pins this scenario to 1M, and the 1M gc numbers below already
tell the story).

| Scale | Base size | Raw loose after 100 | After `git gc` | After `gc --aggressive` | Chunks/commit |
|---|---|---|---|---|---|
| 100k | 18.4 MiB | 898 MiB | 421 MiB (46 s) | 396 MiB (127 s) | 873 |
| 1M | 176.4 MiB | 8,086 MiB | 4,101 MiB (1,032 s) | 4,046 MiB (1,250 s) | 8,749 |

Supporting numbers at 1M: size trajectory is exactly linear (~79 MiB raw
loose per commit); per-commit `apply_batch` mean 3.19 s (spike-only, caveat
1) and `commit_root` mean 81 ms; 918,250 loose objects accumulated before
gc. Both gc passes were run with `--prune=now` (default gc would leave
recent unreachable loose objects in place). Nothing was lost to either gc:
all versions stay reachable from the bench refs.

Two observations worth pulling out:

- **Chunk rewrite amplification is the dominant cost.** 10,000 uniformly
  random updated keys per commit touch ~8,150 of the ~22,700 leaf chunks
  (expected-coverage maths; with rewritten parents it closely matches the
  measured 8,749 chunks/commit),
  so each commit rewrites ~44 MiB of chunks to change ~2.3 MiB of payload.
  Uniform random churn is the worst case for this; real imports with key
  locality would touch proportionally fewer leaves.
- **Git repacking halves it and no more, even aggressive.**
  Packed history settles at ~38.7 MiB/commit — roughly 17× the changed
  payload. Successive versions of a leaf chunk differ by one ~230-byte
  record, so near-perfect deltas exist, but pack-objects pairs delta
  candidates using name/size heuristics and content-addressed blobs have
  no stable name, so it largely fails to find them (`--aggressive`,
  window 250, improved on plain gc by only ~1%). A pack-on-write path
  choosing its own delta bases (the predecessor chunk is known at write
  time) could in principle recover this — but note this is an
  **unvalidated design hypothesis**: gix exposes no repository-level pack
  writer, gitoxide's pack generation does not create new deltas, and stock
  `git repack` cannot be told delta bases, so it means hand-rolling git's
  delta encoding and pack format. Plausible (the format is simple and the
  bases are known) but unmeasured. Relatedly, the roadmap's
  "pack-first writing" arm of this scenario could not be run for the same
  reason — only loose-then-repack was measured.

## Scenario 7 — loose vs packed point reads

Same 1,000-key sample and base root as scenario 2, re-run after full
`git gc --prune=now` (reads then go through gix-pack).

| Scale | Loose p50 | Packed p50 | Loose p99 | Packed p99 |
|---|---|---|---|---|
| 100k | 104 µs | 58 µs | 158 µs | 114 µs |
| 1M | 123 µs | 66 µs | 169 µs | 106 µs |
| 5M | 170 µs | 72 µs | 214 µs | 110 µs |

Packed reads are roughly twice as fast as loose at every scale (fewer file
opens; pack index + mmap). The 5M repack itself (`git gc --prune=now` over
160k objects, no growth history) took 62 s and packed the repo to
646.5 MiB.

## Reading of the evidence

Against the roadmap's Phase 0 exit criteria ("update latency and repo
growth acceptable at 1M keys"), and feeding the Gate A decision:

**Update cost at 1M keys: acceptable.** The architecture-representative
number is chunk writes, and it behaves exactly as designed: 4 chunks
(~20 KiB) per single-key update at 1M, growing only with tree height. The
measured latencies are inflated by a known spike shortcut (caveat 1); the
component costs that a real implementation would pay — 3–5 chunk
reads/writes at ~50–170 µs per loose-object operation — put realistic
single-key update latency around a millisecond, and the 50 ms commit
overhead at 1M (manifest + reachability tree) is paid once per commit, not
per key. Batched updates amortise further: a 10k-key churn batch applies in
~3 s at 1M.

**Read behaviour: comfortably interactive at every scale tested.** Point
reads 104–170 µs loose and 58–72 µs packed (p50); scans at ~900k keys/s
regardless of scale; 1k-key windows in ~1.2 ms. No cliff between 100k and
5M, and 5M fits easily in RAM (~3 GiB peak for the harness).

**Diff: the structural bet pays off.** An OID-comparison walk costs
~2 chunk reads per changed key, independent of map size, and its output was
verified exact at all scales. This is the property the whole design leans
on (status/diff/merge/no-op imports), demonstrated on real git storage.

**Repo growth at 1M keys: acceptable for the workbench use case, but it is
the number to watch, and stock gc does not solve it.** 100 imports at 1%
uniform-random churn grew the repo from 176 MiB to 8.1 GiB loose /
4.0 GiB packed — ~39 MiB/commit retained, ~17× the changed payload. Two
distinct causes, both with identified mitigations: (a) chunk rewrite
amplification (~44 MiB of leaf chunks rewritten to change 2.3 MiB) is
inherent to uniform random churn and much milder under realistic key
locality; (b) git's pack heuristics fail to delta content-addressed chunks
(aggressive repack bought only ~1%), which a pack-on-write path with
explicitly chosen delta bases (the predecessor chunk is known at write
time) may be able to address — an unvalidated hypothesis requiring custom
delta/pack plumbing (no existing gix or git tooling does this; see
scenario 6). If it validates, spec §3.2's pack-first aspiration becomes a
requirement rather than an optimisation where import cadence is high; if
it does not, the fallback positions are import key locality, coarser
import batching, and history compaction/squashing.
Operationally, letting ~918k loose objects accumulate before gc cost a
17-minute gc; periodic incremental repack (or pack-on-write) is needed
hygiene, not optional.

**Feasibility failures: none.** Nothing was infeasible at 5M on this
machine; the growth scenario was simply not run at 5M per the roadmap's
1M pinning.

**Net reading for Gate A**: the evidence supports go on Decision 1
Option A (git ODB as chunk store). Reads, updates, diffs and durability
(everything survives `git gc --prune=now`; all state reachable from refs)
behave as the design predicts at the target envelope. The honest cost is
version-history storage growth under high-churn imports — bounded and
linear, with pack-on-write as the identified but **unvalidated** recovery
path — plus gc/repack operational cost at the million-object scale.
Neither undermines the architecture at workbench scale even unmitigated;
but Phase 1 must treat pack-on-write validation as a named task in
`acetone-store` (with repack policy in `gc`), and Gate A should weigh the
growth number on the assumption the mitigation might not pan out.

## Reproducing

```sh
cd spikes/prolly-git-spike
cargo build --release --bin bench
# each scale gets its own repo under --dir; use a fresh directory
/usr/bin/time -l target/release/bench --keys 100k --dir /tmp/acetone-bench
/usr/bin/time -l target/release/bench --keys 1m   --dir /tmp/acetone-bench
/usr/bin/time -l target/release/bench --keys 5m   --dir /tmp/acetone-bench
```

Budget ~10 GiB of disk (the 1M growth scenario peaks at ~8 GiB before gc)
and ~55 minutes of wall time for all three, of which ~38 minutes is the two
1M gc passes. Data and sampling are fully deterministic (fixed seeds), so
counts and sizes reproduce exactly; timings will vary with hardware.

Scenarios can also be run singly (state persists on `refs/bench/v0` and
`refs/bench/head` inside each repo), e.g.
`bench --keys 1m --dir D --scenarios diff`. See the module docs at the top
of `src/bin/bench.rs` for all flags. Raw harness outputs for the runs
recorded here are attached to the PR that introduced this note.
