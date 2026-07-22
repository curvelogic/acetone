# ADR-0053: `.keep`-mark acetone's consolidation pack so a foreign `git gc` cannot degrade it

*Status: accepted — ratified by Greg at the Phase 8 / 0.3 boundary (2026-07-22) · Date: 2026-07-22 · Bead: acetone-5cw*

## Context

acetone's retention win comes from `GitStore::consolidate` (`acetone gc`, ADR-0011):
it rewrites the reachable object set into one pack whose entries are REF_DELTAs
against predecessors acetone chose at write time, recovering the ~7× that git's
own path/size delta heuristics leave on the table for content-addressed chunk
blobs. Those deltas are the whole point.

But git can undo them. Running stock `git gc` / `git repack -a -d` on an acetone
repository is **safe-but-lossy**: safe in that no object or hash is lost (it is a
valid, complete repack), lossy in that git re-deltifies by its own heuristics and
throws acetone's content-aware deltas away, inflating the pack back toward the
poorly-compressed baseline. Re-running `acetone gc` restores it — but only after
the damage, and only if someone runs it.

Co-tenancy (Phase 8) makes this acute. In the standalone model acetone owns the
repository, so the only `gc` that runs is acetone's own. In a **co-tenant** repo
the *user* owns it and runs `git gc` for their code — and git's **automatic** gc
(`gc.auto`) fires on ordinary operations, silently, without the user choosing to.
So the graph's storage would be de-optimised routinely, as a side effect of the
user just using their own repository. Greg flagged this at the Phase 8 boundary
("it's unpleasant that `git gc` degrades our data storage").

git has a standard mechanism for exactly this: a `<pack>.keep` marker file. `git
repack` (which `git gc` invokes) skips a kept pack by default — it neither
repacks its objects nor deletes it (short of an explicit `--pack-kept-objects`).
acetone was not writing one.

This decision depends on ADR-0051 reading **(B)**: only because `acetone gc` now
produces a **graph-only** pack can we `.keep` it without freezing the user's
*code* object packing. Under the old reading (A) — a repo-global repack — a
`.keep` would have pinned the user's code objects under acetone's control, the
invasive outcome (B) was chosen to avoid.

## Decision

**Write a `<stem>.keep` marker beside every consolidation pack acetone installs,
so a foreign `git gc`/`git repack` leaves acetone's content-aware deltas intact.
acetone manages the kept pack's lifecycle itself.**

- `install_pack` writes `<stem>.keep` after the pack and index are durable
  (and ensures it on the idempotent already-installed path, so a pack from a
  build that predates this ADR gains its marker on the next run). The marker's
  content is a human-readable reason; git ignores it.
- `supersede_packs` removes `<stem>.keep` together with the `<stem>.pack`/`.idx`
  it retires, so a marker never outlives its pack.
- The marker is written in **both** layouts. It costs nothing in standalone (a
  standalone acetone repo equally benefits from git not undoing its deltas), and
  object storage is unchanged — a `.keep` is not an object, so no hash moves and
  ADR-0051's "standalone byte-identical" (object/pack *content*) still holds.

## Consequences

- **A foreign `git gc` no longer degrades the graph's storage.** Proven by a test
  that runs the real `git repack -a -d` on a co-tenant repo after `acetone gc`
  and asserts acetone's pack is byte-unchanged, the repo is `fsck`-clean, and the
  graph still reads — then removes the `.keep` and shows the same repack folds the
  pack away, confirming the marker is load-bearing.
- **acetone owns the kept pack's retirement.** git will never expire a kept pack,
  so acetone must retire it itself — which it already does via `supersede_packs`
  (gated on the successor pack containing every object first). No new leak: the
  pack-stems sidecar still tracks every pack acetone wrote.
- **git's pruning of unreachable objects is unaffected.** A kept pack can hide
  unreachable objects from `git gc --prune`, but acetone's pack only ever
  contains objects reachable from the graph's refs, so there are none to hide.
- **A foreign `git repack` may still leave duplicate storage.** If git packs some
  graph objects into its own (non-kept) pack before acetone next consolidates,
  those objects exist both there and in acetone's kept pack — harmless
  duplication that the next `acetone gc` resolves (it repacks the reachable set
  and supersedes its own prior pack; git's pack is left as git's business).
- **Foreclosed:** nothing. `.keep` is advisory; a user who truly wants git to
  repack acetone's pack can pass `--pack-kept-objects` or delete the marker.
- **Revisit if:** a future need arises to let git co-manage acetone's packs, or
  the duplicate-storage window above proves to matter in practice.
