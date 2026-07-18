# ADR-0045: Pin the shipped 64 KiB chunk profile in the prolly golden suite (additively)

*Status: accepted — ratified by Greg at the Phase 7 / 0.2 boundary review (2026-07-18); originally an agent decision (Gate-D adjacent) flagged for phase-boundary review · Date: 2026-07-16 · Bead: acetone-7bn.18 · Relates: ADR-0024 (Gate D freeze), 7bn.17*

## Context

The prolly byte-exact golden suite (`crates/acetone-prolly/tests/golden.rs`)
exists to catch "two builds disagree on the on-disk format while
`format_version` stayed 1". It pinned roots under `ChunkParams::default()` =
`(1024, 12, 16384)` — the Phase-0 spike / test profile. But a real repository
uses `acetone_graph::repo::default_chunk_params()` = `(1024, 12, 65536)` (what
`acetone init` writes). So the goldens did **not** cover the chunk profile any
shipped repo actually produces (surfaced by 7bn.17, which fixed the misleading
docs; this bead is the real reconciliation).

The bead offered two paths: **(a)** realign `ChunkParams::default()` to 65536
and re-pin, for a single canonical default; or **(b)** add a parallel 65536
golden suite alongside the 16384 one.

## Decision

**Option (b): additively pin the 65536 profile; keep the 16384 goldens and
`ChunkParams::default()` unchanged.**

An empirical check (2026-07-16) was decisive. The existing `golden_inner`
dataset (600 entries, ~43 KiB) forms an **inner** node (height 2, root
`3164ef68…`) at `max_bytes = 16384`, but **collapses to a single leaf**
(height 1, root `b995ce6c…`) at `max_bytes = 65536`: the gear hash
(`mask_bits = 12`) rarely cuts on this low-entropy fixed data, so a chunk grows
until it hits the ceiling. Consequences:

- Option (a) would **reduce** coverage, not just move it: the existing dataset
  no longer exercises inner-node framing at 65536 (it must be enlarged to force
  a split), and the 16384 inner-framing coverage is deleted outright.
- Option (a) also changes the meaning of `ChunkParams::default()` — documented
  as "identical to the Phase 0 spike" and used as the benchmark regression
  baseline (`benches/`, `benches/README.md`) — dragging bench numbers and the
  Phase-0 anchor along for an aesthetic gain.
- Option (b) is strictly additive and maximises the safety net: it pins **both**
  ceilings and **both** inner-framing paths. The new 65536 inner golden uses a
  dataset large enough to (i) force height ≥ 2 and (ii) exercise the 64 KiB
  forced-cut ceiling, which the 16384 suite cannot reach.

`ChunkParams::default()` stays `(1024, 12, 16384)` — the spike/test profile,
which 7bn.17 already documented as **not** the shipped default and which
repo-creating code must never use (it calls `default_chunk_params()`). No
behavioural change, no `format_version` implication (per-repo params live in
the manifest; the shipped profile was already 65536), no bench disruption.

## Consequences

- The golden suite now pins the released 64 KiB profile as well as the 16 KiB
  spike profile — the gap the bead names is closed. The `golden.rs` module doc
  is updated: it no longer flags 7bn.18 as an open follow-up.
- Two profiles are golden-covered. The "`default` ≠ shipped profile" naming
  footgun remains, but is fully mitigated by 7bn.17's docs and by both profiles
  now being pinned; eliminating the naming split (option a) was judged not worth
  its coverage and bench costs at a format-freeze gate.
- Any future change to either profile's boundaries or framing still trips a
  golden and must go through a deliberate `format_version` bump + `migrate` +
  re-pin (unchanged policy).
