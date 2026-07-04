# Pack-on-write validation (acetone-63m.10)

Phase 0 left one number to defuse: 100 import commits at 1% uniform churn
retained ~39 MiB/commit at 1M keys after `git gc` — ~17× the changed
payload — because git's pack heuristics pair delta candidates by path and
size, and content-addressed chunks have neither, so near-perfect deltas
between successive chunk versions are never found
(docs/notes/phase0-benchmarks.md, scenario 6; ADR-0002). The unvalidated
hypothesis: acetone knows each rewritten chunk's predecessor at write time,
so packs written by acetone itself, with explicitly chosen delta bases,
should recover the ratio. This note validates that hypothesis.

**Verdict: validated — 7.1× better than the post-gc baseline at both 100k
and 1M keys, with git-readable packs (bar: ≥5×) — with one important
qualification: the win comes from acetone doing its own pack maintenance.
Stock `git repack`/`gc` must be treated as a lossy operation on this data:
it corrupts nothing, but it discards most of the hand-chosen deltas and
lands back at roughly the stock-gc ratio.**

## Method

Everything lives in the workspace-excluded spike
(`spikes/prolly-git-spike`): a `pack` module, predecessor recording in the
spike store, and two opt-in bench scenarios (`pack-growth`, `pack-probe`).

- **Delta encoder** (`pack::encode_delta`): git's delta format — copy
  opcodes (base offset/length) and insert opcodes (≤127 literal bytes)
  after a varint size header. Greedy matcher: FNV hash of 16-byte blocks
  over the base (git's own window size), forward extension, backward
  extension into pending literals. `apply_delta` implements the inverse;
  the round trip is property-tested and fuzzed, and a 230-byte edit to a
  4 KiB chunk deltas to under 320 bytes.
- **Pack writer** (`pack::write_pack`): streamed v2 pack — header, whole
  entries (zlib) or REF_DELTA entries (20-byte base OID + zlib delta, used
  only when the delta beats the whole object), SHA-1 trailer. A native
  `.idx` v2 writer exists for the probe below. Packs are installed with
  `git index-pack --stdin --fix-thin`, then `git prune-packed`.
- **Predecessor tracking**: the splice logic in `tree.rs` knows which old
  chunks each level rewrites; reused chunks anchor exact positions and the
  fresh chunks between anchors are paired positionally with the replaced
  ones (`record_level_bases`). Trees and the manifest are paired with the
  parent commit's counterparts by path. Unpaired objects are simply stored
  whole — pairing is best-effort by design.
- **Scenario**: the phase 0 growth scenario re-run bit-for-bit (same seeds,
  same op stream: 100 commits × 1% uniform value churn) against a fresh
  repo, one pack per commit; three end states measured: the per-commit
  packs as written, a stock `git repack -a -d` (on a copy), and a *native
  consolidation* — one cumulative pack holding every growth object once
  with its chosen delta, written by the same pack writer, replacing the
  per-commit packs.

Machine and toolchain as in the phase 0 note (M3 Max, macOS 26.5.1,
git 2.48.1, Rust 1.96 `--release`). The loose baseline was re-run fresh at
100k (pristine base, identical op stream) and reproduces phase 0 within 2%;
1M baselines are the phase 0 numbers. The 1M pack run took 37 min wall.

## Findings that shaped the design

1. **gix cannot do this** (re-verified, gix-pack 0.72): pack generation
   only emits whole objects (`output::Entry::from_data` → `Kind::Base`) or
   copies existing on-disk deltas; there is no API to create a new delta.
   Hand-rolling was required, as PR #4 predicted.
2. **git rejects on-disk packs whose REF_DELTA base is outside the pack**
   (`pack-probe`): `cat-file` dies with "failed to validate delta base
   reference", `fsck` errors, `verify-pack` reports an unresolved delta.
   (gix, notably, resolves such bases across packs without complaint.) So
   per-commit packs must be completed by `--fix-thin`, which **appends a
   whole copy of every external base** — roughly 10× the thin pack in
   duplicates that persist until consolidation.
3. **Stock repack discards the deltas.** For objects present in several
   packs (every `--fix-thin` duplicate), `pack-objects` keeps whichever
   representation it finds first — mostly the whole copies (104,521 of
   ~114k objects non-delta after `git repack -a -d` at 100k), and this is
   not usefully steerable. Consolidation has to be acetone's job.
4. **Pack entries must be written in explicit topological order.** Chunk
   creation order is *almost* topological, but a chunk OID can re-appear
   after a content-defined boundary shifts back, and `index-pack
   --fix-thin` dies ("duplicate base") on an in-pack base that sits after
   its child. Hit once in ~886k objects at 1M; never at 100k.
5. **git happily reads delta chains far beyond its own generation limit**:
   the consolidated packs carry chains up to length 100 and pass
   `fsck --strict`, `verify-pack` and a full clone; the depth-50 default
   only constrains pack creation.

## Numbers — 100 commits at 1% churn, retained bytes per commit

Retained/commit = (repo size after 100 commits − packed base)/100. Changed
payload: ~0.22 MiB/commit at 100k, ~2.3 MiB/commit at 1M.

| State | 100k | vs post-gc | 1M | vs post-gc |
|---|---|---|---|---|
| Raw loose (baseline) | 8.67 MiB | 0.5× | 79.1 MiB† | 0.5× |
| `git gc --prune=now` (baseline) | 4.02 MiB | 1× | 39.2 MiB† | 1× |
| `git gc --aggressive` (baseline) | 3.73 MiB | 1.1× | 38.7 MiB† | 1.0× |
| Pack-on-write, per-commit packs (incl. `--fix-thin` duplicates) | 4.83 MiB | 0.8× | 48.0 MiB | 0.8× |
| … after stock `git repack -a -d` | 4.19 MiB | 1.0× | 41.3 MiB | 0.9× |
| … after native consolidation | **0.56 MiB** | **7.1×** | **5.50 MiB** | **7.1×** |
| Thin packs alone (floor, no base duplication) | 0.44 MiB | 9.2× | 4.27 MiB | 9.2× |

† phase 0 numbers (same seeds and op stream; the fresh 100k baseline
reproduced them within 2%, so they compare directly).

Supporting numbers: 8,905 of 9,006 objects per commit stored as deltas at
1M (1,124 of 1,131 at 100k; mean encoded delta 612/495 bytes); building,
indexing and pruning one commit's pack added ~4.8 s at 1M (~0.6 s at 100k)
on top of the spike-grade ~10.5 s apply+commit; consolidation of 885,700
objects (875,161 kept as deltas) took 193 s at 1M, 19 s at 100k. Retained
history after consolidation is ~2.4–2.6× the changed payload — the residual
is chunk-rewrite amplification, no longer delta failure.

## Correctness evidence

At every stage (per-commit packs, stock repack, consolidation):
`git fsck --strict` clean and `git verify-pack` passes. After
consolidation, `git clone --mirror --no-local` (a full object walk over the
wire path) succeeds and the clone reads back **every key of the final
version with values verified against the expected state** (all 100k and all
1M keys). The pack module's tests additionally prove whole, in-pack-delta
and thin-delta entries read back byte-identically through real git, and
that git accepts the native `.idx` writer's output.

## Verdict against the bar

**≥5× over the post-gc baseline with git-readable packs: met — 7.1× at
both scales — provided repack-with-chosen-bases (consolidation) is
acetone's own periodic operation.** The fallback positions (key locality,
coarser batching, compaction) are not needed for this problem, though key
locality remains worth having: rewrite amplification is now the dominant
residual cost, deltas having removed the rest of the 17×.

## Implications for acetone-store

Two caveats for the production port, from review: the measured 7.1× uses
uncapped depth-100 delta chains — the recommended periodic whole anchors
(every ~32 versions) will shave the ratio slightly (rough arithmetic keeps
it above 6×, still past the bar); and the experiment's delta encoder indexes
positions as u32, so a production implementation must guard
`base.len() <= u32::MAX` (or fall back to a whole object) — silent copy-op
truncation on ≥4 GiB bases is exactly the kind of trap a port would hit,
even though chunk max_bytes makes it unreachable here. (63m.1)

- The batched-put path should thread the (new chunk → predecessor) mapping
  out of the prolly splice exactly as the spike does; it is a natural
  by-product, and the store's batched-put API shape already fits.
- Per-commit durability can stay simple (loose objects or a thin-completed
  pack — both git-clean); the ratio is won by a **periodic consolidation
  pack with chosen bases**, which must be acetone's `gc`, and is cheap
  (19 s per 100 commits at 100k, 193 s at 1M). `git gc`/`repack` on an
  acetone repo should be documented as safe-but-lossy.
- Consolidation entries must be emitted bases-first (finding 4) and delta
  chains should be capped (e.g. a whole anchor every ~32 versions) to
  bound point-read cost; the experiment's depth-100 chains read fine but
  are the extreme.
- The delta encoder is ~150 lines with two small dependencies
  (flate2/crc32fast); nothing needs gix support, and none of it changes
  the on-disk format seen by readers — packs remain plain git.

## Reproducing

```sh
cd spikes/prolly-git-spike && cargo build --release --bin bench
target/release/bench --keys 100k --dir /tmp/pow --scenarios pack-growth,pack-probe
target/release/bench --keys 100k --dir /tmp/pow-loose --scenarios bulk-load,growth
target/release/bench --keys 1m --dir /tmp/pow1m --scenarios pack-growth
```

Deterministic seeds; counts and sizes reproduce exactly. Budget ~45 min and
~12 GiB of disk for all three; raw outputs are attached to the PR.
