# ADR-0082: A merged discriminator change is a conflict, judged by provenance

*Status: accepted (Phase 13, `acetone-7qw.26`) · Date: 2026-10-26*

## Context

A relationship type's discriminator decides what an edge key's last slot
means. `check_rel_disc_stability` therefore refuses to change it while
edges of that type exist (PR #243). That check sees only one workspace.
A branch with no edges of the type can legally declare, rename or drop
the discriminator. Meanwhile another branch writes edges under the old
declaration, and the merge pairs the two without any check.

The results are silent. A `Null`-keyed edge under a newly declared
discriminator keeps a record value under the declared name. Reads shadow
that value (the key wins) and the next write erases it. After a rename,
the key's value is read as a different property. After a drop, the value
becomes unreachable.

ADR-0066 set the pattern for this kind of gap. A breach that only the
merged state composes is reported as a graph violation, not as an error:
conflicts are data (ADR-0007), commit is refused, and the workspace is
repaired by ordinary writes.

## Decision

1. **Surface it, don't refuse it.** `validate_merged` reports
   `GraphViolation::StaleDiscriminator { edge, declared, minted }` for
   each such edge. `CALL acetone.conflicts()` shows it with kind
   `rel-stale-discriminator`: the relationship type is in `label`; the
   declared discriminator is in `property` and `base` (expected); the
   discriminator the edge was written under is in `ours` (found); none
   is null. The report and the daemon's `merge` reply pick it up through
   the same rows. Completion is refused until the edge is deleted. It
   can then be recreated under the merged declaration, with a new key.
2. **Judge by provenance, not key shape.** Because of the stability
   check, every edge in a committed manifest was written under that
   manifest's discriminator for its type. The merge is responsible
   exactly when a side's discriminator for a type differs from the
   merged schema's. In that case every edge of that type which the side
   holds and the merged graph keeps is reported. An undeclared type
   counts as declaring none. Key shape (`Null` or not) would catch a new
   declaration and a drop, but not a rename, whose values look valid
   under either name. So `validate_merged` now takes `ours` and `theirs`.
   Completion and `Repository::conflicts` load them from the branch tip
   and `MERGE_HEAD`, which they already resolve.
3. **Cost.** The check reads each side's schema. It scans a side's
   forward edges, with a point lookup per edge of a disagreeing type,
   only when that side disagrees with the merged schema.

## Consequences

- **Format: none.** The conflicts-map entry uses graph tag 5
  (`[1][5][declared][edge]`). Graph entries are advisory markers whose
  details are never decoded (ADR-0058), so older readers treat it as any
  marker. `format_version` stays 1. CHANGELOG 0.5's `rel-wrong-type` is
  the precedent.
- **API.** The public `GraphViolation` enum gains a variant. This is
  additive, but the enum is not `#[non_exhaustive]`, so an exhaustive
  downstream match breaks. The ADR-0066 note about the freeze gate's
  blindness to variants applies here too.
- **Determinism (Invariant 4).** The violations are a pure function of
  base, ours, theirs and merged. They are emitted in edge-key order, each
  edge once.
- **Residual.** An edge the workspace rewrites during a rename merge
  under the same key value is indistinguishable from the stale one and
  stays reported. The repair is to delete it or migrate it. Two
  declare-time gaps sit outside this ADR: declaring a discriminator for
  a type that is new to the schema over edges already written under it,
  and deleting a relationship-type entry. Neither is checked by
  `check_rel_disc_stability`.
