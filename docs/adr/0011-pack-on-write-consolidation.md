# ADR-0011: Production pack-on-write via periodic gc consolidation

*Status: accepted · Date: 2026-07-04 · Bead: acetone-63m.13*

## Context

Content-addressed chunks have no stable path and no size correlation, so
git's own delta heuristics never pair a rewritten chunk with its
predecessor: 100 import commits at 1% churn retained ~17× the changed
payload even after `git gc` (Phase 0 scenario 6, ADR-0002). The
pack-on-write experiment (bead acetone-63m.10,
`docs/notes/pack-on-write-validation.md`) validated the fix: acetone knows
each rewritten chunk's predecessor at write time, and a pack whose entries
are REF_DELTAs against those hand-chosen bases recovers a **7.1× better
retention ratio** than the post-gc baseline at both 100k and 1M keys, while
staying plain, git-readable packs. The experiment left three things to
settle for production: how the predecessor mapping leaves the prolly layer,
where the delta-choosing pack maintenance lives, and what guards a hostile
or merely large input needs.

Two hard constraints shape the production port that the spike did not face:

- **The store never shells out to git.** `GitStore` opens repositories
  reduced-trust with gix's `command` feature disabled precisely so no code
  path spawns a process (`crates/acetone-store/src/git.rs` module docs).
  The spike installed packs with `git index-pack --fix-thin` and pruned
  with `git prune-packed`; production cannot.
- **The store already supports SHA-1 and SHA-256 repositories.** The
  spike's pack and index writers hard-coded SHA-1.

## Decision

Split the work exactly as the validation note recommends: per-commit
durability stays as loose objects (git-clean, already implemented); the
retention win is won by a **periodic consolidation pack with chosen delta
bases**, which is acetone's own `gc` and lives in `acetone-store`.

1. **Predecessor mapping out of prolly.** `acetone-prolly` gains
   `apply_batch_recording`, which returns the new root *and* a
   `Vec<(new_chunk, predecessor)>` discovered during the splice. It is
   threaded as an optional observer through `build_levels`/`chunk_level`;
   the existing `apply_batch` delegates to it with a no-op recorder, so the
   reviewed tree code and its bit-identical output are unchanged. Pairing
   ports the spike's positional `record_level_bases`: within each
   reuse-bracketed run of a rebuilt level, fresh chunks pair positionally
   against the old nodes they replace. It is **best-effort by design** — an
   unpaired or mispaired chunk merely costs a delta, never correctness.

2. **Self-contained consolidation pack, no thin completion.** Consolidation
   enumerates the reachable object set `S` (rev-walk of the kept refs, tree
   traversal per commit) and writes **one** version-2 pack over `S`. A base
   hint is used only when the base is itself in `S`, so every REF_DELTA base
   is in the pack: the pack is self-contained and valid to git with no
   `--fix-thin` pass and therefore no git subprocess. The pack and a native
   v2 index (the spike's writer, extended to the large-offset table) are
   written straight into `objects/pack`. Entries are emitted **bases-first**
   in a topological order over the hint DAG restricted to `S`; because `S`
   is a set, the duplicate-OID hazard the spike hit (finding 4) collapses to
   a single node, and a cycle or a base ordered after its child falls back
   to a whole entry.

3. **Production guards.** (a) The delta encoder indexes positions as `u32`;
   a base of `len > u32::MAX` is never deltified (whole fallback), closing
   the silent copy-op truncation the note flagged. (b) **Every emitted
   REF_DELTA is validated** — `apply_delta` must reproduce the object's
   exact bytes — before it is written; any mismatch falls back to a whole
   entry. This makes representation-only invariance robust to *any* bad base
   hint, not merely the ones we anticipated. (c) Delta chains are capped with
   a whole anchor every 32 links to bound point-read cost (the note's
   depth-100 chains read fine but are the extreme).

4. **Pruning gated on the new pack.** After the pack is written, loose
   object files are deleted only for OIDs the new pack's index actually
   contains, and prior acetone consolidation packs (tracked in a local
   sidecar) are deleted only when every one of their OIDs is in the new
   pack. Nothing can be pruned that was not first preserved.

5. **Base hints persist locally.** `GitStore::record_base_hints` appends
   `(new, base)` pairs to `<common_dir>/acetone-pack-bases`. This is a
   local optimisation cache, not transferable state: it is not a ref and
   does not travel with clone/push (consistent with the operational
   constraint that transferable state lives only in `refs/heads|tags`). A
   torn or absent log costs delta quality, never correctness — consolidation
   simply stores more objects whole.

## Consequences

- **Recovers the ratio without a git subprocess.** Self-containment buys
  git-valid packs and drops both the `--fix-thin` base duplication and the
  shell-out; the price is that a chunk whose predecessor is no longer
  reachable (truncated history) is stored whole for that one version.
- **`git gc`/`git repack` on an acetone repository is safe-but-lossy** and
  is documented as such: it corrupts nothing, but discards the hand-chosen
  deltas and lands back near the stock-gc ratio. Re-running acetone's own
  `consolidate` restores it. (Post-consolidation `git gc` actually preserves
  the ratio, since the deltas are already in a pack it keeps, but the
  blanket caveat is the safe guidance.)
- **Representation-only, provably.** Consolidation writes each object's
  exact bytes, so every OID — and therefore every prolly root hash and the
  five load-bearing invariants above them — is unchanged; the delta
  validation and prune-gating make losing or altering a reachable object
  impossible by construction.
- **Costs two small dependencies** in `acetone-store`: `flate2` (configured
  to use the `zlib-rs` backend already in the tree via gix, so no second
  zlib implementation is pulled) for pack entry compression, and
  `crc32fast` (already a transitive dependency) for index CRCs. gix cannot
  create new deltas (re-verified, finding 1), so a hand-rolled writer is
  unavoidable.
- **Deferred:** consolidating repositories whose objects are already in
  non-acetone packs (those objects are duplicated into the new pack but the
  old pack is not pruned); wiring the recorder through the graph layer's
  commit path and choosing the gc cadence (Phase 5, when scheduled-import
  cadence makes retention matter). This bead ships the library machinery and
  its tests, not a CLI command.
- **Deferred scaling (bead acetone-627), correctness-neutral:** the base-hint
  sidecar grows O(all rewrites) and `load_hints` reads it whole; and the pack
  and its index are assembled in memory rather than streamed to disk. Both
  bound a very large whole-history consolidation by RAM but change nothing
  about representation-only correctness. Compaction of the sidecar (dropping
  hints whose endpoints are unreachable) and an incremental pack/index writer
  are tracked there.
