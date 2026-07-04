# ADR-0007: Conflicted keys are excluded from the merged root

*Status: accepted · Date: 2026-07-04 · Bead: acetone-63m.2 · PR: #12*

## Context

Spec §3.2 requires `merge(base, ours, theirs)` to return "merged root plus
a stream of key-level conflicts", and Load-Bearing Invariant 4 makes the
merge a pure function with conflicts as data. Neither says what value a
*conflicted* key holds **in the merged tree itself**. A surface reading of
spec §6 ("the workspace enters merging state; the user resolves by
ordinary writes") is compatible with three choices: keep base's value,
keep ours, or keep neither. The choice is format-visible — it changes the
merged root hash — so it must be decided once, deliberately.

## Decision

A key with divergent changes on both sides (delete counts as a change) is
**absent from the merged root**. Its full context — `(key, base value,
ours, theirs)`, any of which may be "absent" — is delivered only in the
key-ordered conflict stream. No resolution of any kind is applied at this
layer. Per-key semantics otherwise: change on one side is taken;
identical change on both sides (including both deleting) is taken once.

Rationale:

- **Determinism without bias.** Keeping "ours" makes the merged root
  depend on argument order in a way that silently discards the other
  side; keeping "base" silently resurrects data both sides intended to
  change. Exclusion is the only symmetric option.
- **The caller owns materialisation.** Spec §6 places resolution in the
  graph layer (Phase 4, acetone-graph): it populates the `conflicts` map,
  and `resolve --all-ours/--all-theirs` are bulk operations over the
  conflict records, which carry every datum needed for any policy.
- **Wrong reads fail loud.** During merging state a read of a conflicted
  key finds nothing, rather than a value that pretends the merge was
  clean.

## Consequences

A merge with a non-empty conflict stream is **incomplete by contract**:
callers must not treat its root as a usable version until every conflict
is resolved by a subsequent batch (documented on `merge`; the graph
layer's merging-state machinery is the intended consumer). Property tests
pin the contract: conflicted keys absent from the merged root, conflict
records carrying exactly (base, ours, theirs), stream key-ordered, merged
root bit-identical to a fresh build of the reference merge. Foreclosed:
callers cannot distinguish "conflicted" from "absent" by reading the
merged tree alone — they must consult the conflict stream, which is the
point. Revisit only if Phase 4 finds the graph-level `conflicts` map
needs the storage layer to carry an in-tree marker instead; that would be
a format change (spec §10).
