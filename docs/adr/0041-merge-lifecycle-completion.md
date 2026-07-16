# ADR-0041: Merge lifecycle — abort, graph-violation resolution, and completion re-validation

- Status: accepted
- Date: 2026-07-16
- Bead: acetone-mws (folds in acetone-36y)
- Supersedes the "graph violations are not persisted / leave the repository unchanged" slice of ADR-0020.

## Context

ADR-0016 made a `Merged` commit both map-clean and graph-valid: an otherwise
clean merge that introduces a dangling edge or breaches a constraint is demoted
to `MergeConflict::Graph(GraphViolation)`. ADR-0020 then persisted **cell**
conflicts as a merge-in-progress workspace (`MERGE_HEAD` + a `conflicts` map),
but deliberately left **graph** violations unpersisted — "leave the repository
unchanged and merely report them" — because there was no way to resolve them
and no way to back out of a wedged merge.

That left three gaps (PR #61 review): a graph-violation merge had no path to
completion; there was no escape hatch (`merge --abort`) for any merge; and a
merge *completion* commit (after resolving cell conflicts) was **not**
re-validated — a resolution could itself introduce a dangling edge, drop a
required property, or create a UNIQUE collision and commit it silently
(acetone-36y). ADR-0035 (cell-wise merge) sharpened the last point: an
auto-merge can now one-sidedly delete a required property.

## Decision

**1. Both cell and graph conflicts enter merge-in-progress.** `Repository::merge`
persists the partial merge and sets `MERGE_HEAD` regardless of conflict kind.
For a graph violation the merged manifest is map-complete (the maps merged;
validation flagged the *resulting graph*), so the workspace shows that graph and
the `conflicts` map lists the violations to repair.

**2. Graph violations resolve by ordinary writes; the gate is completion
re-validation.** There is no "pick a side" for a dangling edge. The user repairs
the graph directly (delete the dangling relationship, restore the endpoint, fix
the constraint breach). Graph-violation `conflicts`-map entries are *advisory*
(they surface in `status` / `acetone.conflicts` / the merge report) and are
**not** cleared by a write. The real gate is at commit: while `MERGE_HEAD` is
set, `Transaction::commit` re-runs `validate_merged(base, resolved-workspace)`
over the merge base and refuses to complete while any violation remains; when
clean it drops the advisory entries and lands the two-parent commit.

**3. Completion re-validation also covers cell-conflict merges (acetone-36y).**
The same re-validation runs for *every* merge completion, not just graph-violation
ones — so a cell resolution (or a cell-wise auto-merge) that leaves a dangling
edge, a missing required property, or a UNIQUE collision is caught at commit
rather than committed silently. Unresolved *cell* conflicts are still gated by
their `conflicts`-map entries (each resolving write clears its entry); a
remaining cell entry refuses the commit before re-validation runs.

**4. `merge --abort` is the escape hatch.** `Repository::abort_merge` resets the
workspace to the branch tip's manifest (dropping the partial merge and its
conflicts map) and clears `MERGE_HEAD`. It is the only way to back out of a
graph-violation merge, and works for cell merges too. It is **idempotent**: a
merge is abortable while `MERGE_HEAD` is set *or* the workspace still carries a
conflicts map, so a partial abort (a failed `delete_ref` or workspace CAS) is
recovered by simply re-running `merge --abort`.

**5. Defensive `MERGE_HEAD` handling.** A `MERGE_HEAD` already in the branch
tip's history is stale (a prior completion whose `delete_ref` failed); commit
does not add it as a second parent and clears it, and a clean merge clears any
stale `MERGE_HEAD` so a later ordinary commit is not turned into a spurious
merge commit.

## Consequences

- The conflict story is complete: every merge either fast-forwards, commits
  clean, enters an in-progress state that can be resolved (cell) or repaired
  (graph) and completed, or is aborted.
- Completion re-validation is `validate_merged` — the same pure, deterministic
  function used at merge time — so no new invariant surface; it runs once per
  completion commit (an `O(merged-graph scan)` cost, paid only mid-merge).
- No format change: `MERGE_HEAD`, the `conflicts` map, and the re-validation are
  all workspace-only. The graph-violation entry-key encoding is unchanged from
  ADR-0020.
- A completing merge always has a branch tip and a merge base (the branch is
  frozen at `ours` for the whole merge-in-progress), so their absence at
  completion means a corrupt or injected `MERGE_HEAD`: `commit` refuses
  (`NoMergeBase`) rather than joining an unrelated history unchecked.
- **Known boundaries (v0.2):** `status`/`acetone.conflicts` show the *persisted*
  graph-violation entries, which are not re-derived after a repair — so they can
  over-report until the completion commit clears them; the authoritative check is
  commit-time re-validation. Interactive per-violation resolution UX is not
  attempted.
