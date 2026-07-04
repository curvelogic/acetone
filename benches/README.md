# benches — `acetone-bench`

Regression benchmarks against the real crates, per
`docs/acetone-03-roadmap.md` ("the Phase 0 suite kept alive afterwards as
regression benchmarks"). The Phase 0 spike suite
(`spikes/prolly-git-spike/src/bin/bench.rs`, bead acetone-28x.4; numbers in
`docs/notes/phase0-benchmarks.md`) is ported here to run against
`acetone-store` (`GitStore`) and `acetone-prolly` (bead acetone-63m.9).

## Scenarios

The roadmap's representative asset-registry envelope, in run order:

| scenario | measures |
|---|---|
| `bulk-load` | load time, throughput, tree height, chunks written, loose repo size |
| `point-read` | point-lookup latency distribution (loose) |
| `scan` | full-scan throughput + random 1k-key window latencies |
| `update` | single-key update latency and **chunk-write amplification** |
| `diff` | `apply_batch` a 1% churn batch, then stream `prolly::diff`; verify the changed set |
| `growth` | 100 simulated import commits at 1% churn; repo-size trajectory and `git gc` |
| `repack-read` | point reads after `git gc`, through the packed ODB |

`bulk-load` always runs first (it creates the repo the others read).

## Asserted vs printed

Wall-clock throughput/latency are machine-dependent and are **printed** for
manual runs, never asserted. The **asserted**, machine-independent
regressions double as invariant guards:

- a full scan visits every key; window scans return their exact size;
- `diff` reports exactly the churned keys — and, at small scale, the same set
  a naive two-scan diff computes;
- **history independence** (Load-Bearing Invariant 1): a tree reached via
  `apply_batch` has the same root hash as a fresh `bulk_load` of the resulting
  contents;
- single-key update write amplification stays within the root→leaf spine
  (`≤ height + 2` chunks).

## Running

Full-scale manual runs (budget disk and time as in the phase 0 note):

```sh
cargo run -p acetone-bench --release -- --keys 100k --dir /tmp/acetone-bench
cargo run -p acetone-bench --release -- --keys 1m   --dir /tmp/acetone-bench
cargo run -p acetone-bench --release -- --keys 5m   --dir /tmp/acetone-bench
# a single scenario against a fresh repo:
cargo run -p acetone-bench --release -- --keys 100k --dir /tmp/b --scenarios diff
```

CI runs the tiny smoke path, which exercises every scenario and every
assertion in a couple of seconds against a throwaway repo:

```sh
cargo test -p acetone-bench          # the smoke integration test
cargo run  -p acetone-bench -- --smoke
```

The O(n) cross-checks (naive scan diff, history-independence rebuild) default
on up to 200k keys and can be forced with `--verify` / disabled with
`--no-verify`.

## Not ported: pack-on-write

The spike's `pack-growth`/`pack-probe` scenarios are **not** here. They rely
on a hand-rolled delta encoder, pack/index writers and predecessor tracking
threaded out of the prolly splice — none of which exists in the production
crates yet. The hypothesis is already validated in the spike and
`docs/notes/pack-on-write-validation.md`; the production port is bead
acetone-63m.13, and the pack-on-write regression benchmark should follow it.
