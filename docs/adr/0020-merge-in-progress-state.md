# ADR-0020: Merge-in-progress state and conflict persistence

*Status: accepted (agent decision, flagged for phase-boundary review) · Date: 2026-07-07 · Bead: acetone-14c.4 (part a)*

## Context

A three-way merge that does not resolve cleanly must not commit. Spec §6
says the workspace instead "enters merging state: the `conflicts` map is
populated … the user resolves by ordinary writes plus `acetone resolve …`;
`acetone commit` completes the merge." acetone-14c.2 shipped `merge` returning
`MergeOutcome::Conflicts` but *without* persisting anything — the repository
stayed on `ours`. This ADR records how the merge-in-progress state is
persisted and completed.

## Decision

### The state: a partial-merge workspace + `MERGE_HEAD`

On a **cell**-conflicted merge the workspace becomes the **partial merge** —
every non-conflicted key merged in, conflicted keys absent — with the
manifest's `conflicts` map populated. A per-worktree ref
`refs/worktree/acetone/merge-head` names `theirs` (a `MERGE_HEAD` equivalent).
The branch does **not** move. Presence of `MERGE_HEAD` is the "merge in
progress" signal.

**Graph-level violations are not persisted in this slice.** They cannot be
picked a side, there is no by-write resolution or abort verb yet
(acetone-14c.4c), so persisting them would wedge the workspace with no exit.
Until 14c.4c ships resolution + abort, a graph-violation merge leaves the
repository unchanged and merely reports the violations (as before this bead).
Conflicts are homogeneous (cell XOR graph), so `merge` persists only when
every conflict is a cell conflict.

### The conflicts map records *which* keys conflict, not their values

The `conflicts` prolly map stores one empty-valued entry per conflict, whose
**key** encodes `[kind][…]`:

- cell conflict → `[0][map-tag][original-key]` (map-tag: schema/nodes/edges);
- graph violation → `[1][class][primary-entity-key]`.

Conflict **values are not stored**. `ours` is the branch tip and `theirs` is
`MERGE_HEAD`, both immutable commits, so a conflict's base/ours/theirs values
are re-derived on demand by probing those manifests. This keeps the map a
compact, deterministic index (Invariant #2) and avoids a second value
encoding to version.

### Resolution

`acetone resolve --all-ours|--all-theirs` (`Repository::resolve_all`) takes,
for every cell conflict, the conflicted key's value from the chosen side's
manifest (put it, or delete if absent there), maintaining `edges_rev`, then
drops the `conflicts` map. Graph-level violations cannot be picked a side and
are rejected here — they are resolved by ordinary writes (acetone-14c.4c).
Per-key resolution and by-write clearing also arrive in 14c.4c.

### Completion

`acetone commit` (`Transaction::commit`) completes the merge when `MERGE_HEAD`
is set: it refuses while the `conflicts` map is non-empty, otherwise writes a
**two-parent** `[ours, theirs]` commit and deletes `MERGE_HEAD`. Because a
completing merge always records history, the CLI's no-change guard is skipped
while merging (a `--all-ours` resolution can leave the graph equal to `ours`
yet must still commit the merge). A new `RefStore::delete_ref` (idempotent)
clears `MERGE_HEAD`.

## Consequences

- The full loop works end-to-end: `merge` → merge-in-progress → `resolve
  --all-ours|--all-theirs` → `commit` (two-parent), with `status` reporting the
  in-progress merge and remaining conflict count; `fsck` clean afterwards.
- The `conflicts` map lives only in the local workspace manifest (never
  pushed), so its encoding is not history format — but it is still canonical
  and deterministic.
- **Scope (deferred to acetone-14c.4b/c):** `CALL acetone.conflicts()` and the
  `_Conflict` virtual subgraph (inspection), per-key `acetone resolve <key>`,
  by-ordinary-write resolution, graph-violation resolution, and aborting a
  merge. `MERGE_HEAD` and the `conflicts` map are the shared substrate they
  build on.
- **Flagged for the Phase 4 boundary:** the merge-state model (a
  `MERGE_HEAD` ref + workspace conflicts map, mirroring git's merge state) and
  the not-storing-values choice.
