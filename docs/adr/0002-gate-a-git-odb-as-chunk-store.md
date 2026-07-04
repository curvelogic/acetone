# ADR-0002: Gate A — git object database as the chunk store

*Status: proposed — Gate A is Greg's call; ratified when bead acetone-28x.6 is
closed at the Phase 0 boundary · Date: 2026-07-04 · Bead: acetone-28x.6 ·
Evidence: PRs #4, #5, #6*

## Context

Decision 1 of the design record bets the architecture on a real git
repository as the content-addressed chunk store (Option A), with a hybrid
native-store design (Option C) as the escape hatch. Phase 0 existed solely to
retire this risk before anything was built on it, with three questions to
answer in writing: go/no-go on Option A; gitoxide vs git2 for the write path;
and adopt/fork/build for the prolly tree.

## Recommendation

**Go on Option A**, behind the `ChunkStore` trait, with Option C retained as
the designed escape hatch. **gitoxide (gix ≥ 0.85)** as the sole git
substrate — the spike needed no git2 fallback on any path it exercised.
**Build `acetone-prolly` from scratch**, treating the prollytree crate as
reference material only.

## Evidence

- **Feasibility (PR #4)**: a prolly map over the git ODB with git blob OIDs
  as the single addressing scheme works end to end; commits on
  `refs/spike/*` keep every chunk reachable — data survives
  `git gc --prune=now`, `git clone --mirror` and `git fsck --strict`.
- **History independence (PR #5)**: five-property proptest suite; no failure
  against the real implementation; review mutation-testing caught 3/3
  behaviour-changing seeded bugs. The invariant is designed in, not hoped
  for.
- **Performance at the target envelope (PR #6)**: 1M-key bulk load 6.0 s;
  point reads 58–170 µs p50; scans ~900k keys/s; single-key update touches
  exactly the O(log n) spine (4 chunks at 1M); structural diff costs ~2 chunk
  reads per changed key independent of map size, verified exact. 5M keys
  fully feasible (~2.9 GiB RSS).
- **The honest cost (PR #6)**: version-history growth under high-churn
  imports — 100 commits at 1% uniform-random churn retained ~39 MiB/commit
  (~17× changed payload); stock git repacking recovers only half, because
  pack heuristics cannot pair content-addressed blobs; gc at ~918k loose
  objects took 17 minutes. The identified recovery path (pack-on-write with
  chosen delta bases) is an **unvalidated hypothesis** requiring hand-rolled
  delta/pack plumbing — tracked as bead acetone-63m.10, to be validated
  early in Phase 1, with named fallbacks (import key locality, coarser
  batching, history compaction).
- **prollytree (PR #3)**: disqualified with running-code evidence — released
  versions fail history independence; its git backend leaves chunks
  unreachable (destroyed by gc, lost on clone); diff does not recurse; no
  range scans.
- **gix findings (PR #4)**: 0.85+ required (RUSTSEC-2025-0140 on older
  gix-date); pulls `uluru` (MPL-2.0) via gix-pack — licence allowlist
  decision needed from Greg; no repository-level pack-first writing exists,
  hence the pack-on-write question above; plain `git clone` does not
  transfer non-standard ref namespaces — the spec §3.5 ref layout should
  note transfer behaviour.

## Consequences

The entire git ecosystem (clone, push, hosting, review, signing, CI) comes
for free, as designed. The storage engine's worst number is history growth
under churn — acceptable at workbench scale even unmitigated, but Phase 1
must treat pack-on-write validation as a named, early task and the repack
policy as required hygiene, not optional. `ChunkStore` remains the seam that
makes all of this reversible; nothing above the trait changes if Gate A is
ever revisited.
