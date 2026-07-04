# Operational constraints

Environment facts that shape design and workflow decisions. Add entries as
they are discovered; each states its consequence, not just the observation.

## Git proxies may refuse non-standard ref namespaces (2026-07-04)

Observed by Greg in Claude Code web environments: the git proxy passes
`refs/heads/*` and `refs/tags/*` but returns 403 for `refs/dolt/*`.
Infrastructure (proxies, hosting ACLs, CI mirrors) can *actively refuse*
custom namespaces — a stronger constraint than "default refspecs don't
fetch them".

**Consequences:**

- Acetone design rule (recorded on bead acetone-63m.5, to be reflected in
  the spec §3.5 revision): anything that must survive transfer lives in
  `refs/heads/*` or `refs/tags/*` only. Custom namespaces
  (`refs/acetone/workspaces/*`) may hold only state that is correct to lose
  on clone and never needs to cross a proxy. Workspace/WORKING state
  qualifies; conflict state and import bookmarks must not rely on
  custom-namespace sync.
- Beads sync (`refs/dolt/data`) fails in such environments — treat `bd`
  Dolt sync as desktop-only for this repository.

## Stock `git gc`/`git repack` on an acetone repo is safe-but-lossy (2026-07-04)

Acetone's retention win depends on packs whose deltas it chose itself
(`GitStore::consolidate`, ADR-0011, bead acetone-63m.13). Content-addressed
chunks defeat git's own delta heuristics, so **running stock `git gc` or
`git repack` on an acetone repository discards the hand-chosen deltas** and
lands back near the un-deltified baseline (roughly 7× more retained history).

**Consequences:**

- It corrupts nothing — every object still reads back and `git fsck` stays
  clean — so operators and tooling may run `git gc` safely; it only costs
  space. Re-running `acetone`'s own consolidation restores the ratio.
- The base-hint cache (`<git-dir>/acetone-pack-bases`) and the consolidation
  pack list (`<git-dir>/acetone-consolidation-packs`) are **local**: they are
  not refs and do not travel with clone/push/fetch. A clone therefore starts
  with no hints and relies on the deltas already baked into the transferred
  pack; losing either file only makes the next consolidation store more
  objects whole, never wrong.
