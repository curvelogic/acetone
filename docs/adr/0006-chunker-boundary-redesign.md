# ADR-0006: Chunker boundary test — entry-end, size-weighted, fan-out ≥ 2

*Status: accepted · Date: 2026-07-04 · Bead: acetone-63m.2 · PR: #12*

## Context

The Phase 0 spike (and PR #12's first draft, which ported it) tested the
gear-hash boundary condition `hash & (2^mask_bits − 1) == 0` at **every
byte** of the serialised entry stream, cutting after the entry containing
a hit. A gear hash updated as `h = (h << 1) + gear[b]` carries each input
byte's influence on the low `mask_bits` bits for only `mask_bits` further
bytes — the decision window is the trailing bytes alone.

At internal tree levels, an entry is `(last_key, child_hash)` and the key
bytes are **identical at every level** for the same span of the map. When
a key is long enough that eligible cut positions (past `min_bytes`) fall
inside its constant bytes, the cut decision there is the same at every
level. Two inner entries with ≥ ~64-byte keys can then split into two
chunks identically at every level: a **deterministic fixed point** in
which no level has fewer chunks than the one below, and tree construction
never converges on a root. The spike would have looped forever (it has no
height bound); it was never caught because its property suite only
exercised default parameters (`min_bytes = 1024` pushes the eligible
region out of reach for its ≤ 80-byte test keys). The new
non-default-parameter convergence property required by this bead found it
within 17 cases; the shrunk counterexample is committed as a
`.proptest-regressions` seed and a deterministic long-key regression test.

## Decision

1. **Evaluate the boundary condition once per entry, at the entry's
   end.** An inner entry ends with the child hash, so the decision window
   always covers bytes that change from level to level — the fixed point's
   precondition (a decision made entirely from level-invariant bytes) is
   removed.
2. **Weight the test by entry size**: cut when
   `hash & (2^mask_bits − 1) < entry_len`. Each entry cuts with
   probability ≈ `entry_len / 2^mask_bits`, keeping the expected chunk
   size byte-denominated (~`min_bytes + 2^mask_bits`, the same ~4 KiB-mean
   tuning as the spike and spec §3.2) whatever the entry-size mix.
3. **Never cut a chunk with fewer than two entries.** Inner fan-out is
   therefore ≥ 2 and any build of `n` entries converges within `log2(n)`
   levels *by construction*, even under degenerate-but-valid parameters.
   A `MAX_HEIGHT = 64` bound remains as defence in depth (and caps hostile
   root descriptors).

The gear table, per-chunk state reset, `min_bytes`/`max_bytes` clamps and
parameter surface are unchanged.

## Consequences

Chunk boundaries differ from the spike's — every spike hash is obsolete.
This is free: the format is pre-freeze (spec §10) and the spike was
explicitly throwaway; nothing normative referenced its hashes. Convergence
is now guaranteed rather than probabilistic, and history independence is
property-tested under non-default parameters, not just defaults.
Cut-point granularity is per-entry rather than per-byte, which for very
large entries (larger than `2^mask_bits`) means an almost-certain cut —
acceptable, since such an entry dominates its chunk anyway. Adversarial
content can still skew chunk sizes toward `min`/`max` (the decision
window is the trailing bytes), but never break determinism or
convergence. Revisit if format-freeze benchmarking (Gate D) shows the
size distribution hurting dedup or pack locality.
