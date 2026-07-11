# ADR-0019: Node blame follows the first-parent chain

*Status: accepted — ratified by Greg at the pre-0.1 boundary review (2026-07-11); originally an agent decision flagged for phase-boundary review · Date: 2026-07-07 · Bead: acetone-14c.6*

## Context

`CALL acetone.blame(label, key)` (spec §5.2) returns "the sequence of commits
whose diffs touch the node's key, computed by walking the commit graph and
probing the node map path (O(log n) per commit)". "Walking the commit graph"
leaves the traversal order unspecified when history has merges — and the
order determines which commit is credited with a change that arrived on a
side branch.

Two reasonable readings:

- **First-parent chain** (what `git blame --first-parent` does): walk only
  first parents from HEAD. A change merged in through a two-parent merge is
  credited to the *merge commit* (its record differs from its first parent).
- **Full history follow** (git's *default* blame): descend into merge parents
  to credit the branch commit that actually authored the change.

## Decision

Blame walks the **first-parent chain** from HEAD, comparing each commit's
record of the node to the next-older commit's (canonical-CBOR equality),
crediting the commit on a difference — introduction, property change, or
deletion.

Rationale:

- It is deterministic and `O(log n)` per commit (one prolly `get` over the
  manifest's `nodes` root), matching the spec's cost claim, with no
  merge-base or reachability computation.
- Every value the node holds on the surviving (first-parent) history is
  attributed to a real commit. A side-branch change identical to what the
  merge's first parent already held is not *separately* credited — but that
  value is still attributed to the first-parent commit that set it, so **no
  node state goes unattributed** (verified adversarially).
- It matches a well-known git convention (`--first-parent`), so the semantics
  are predictable.

## Consequences

- A change authored on a side branch is blamed to the **merge commit** that
  brought it onto the first-parent line, not to the original branch commit.
  This is coarser than git's default blame; it is the intended, documented
  behaviour, not a bug.
- No merge-base or full-graph walk is needed, keeping blame cheap.
- **Flagged for the Phase 4 boundary:** if per-author attribution into merge
  parents is later wanted, it is an additive refinement over the same
  `record_at` probe — the first-parent result is a correct subset-ordering of
  the same change set.
- Follow-up (acetone-596 filed): the CLI `acetone.blame` accepts single-column
  keys only and returns empty (rather than erroring) for a multi-column
  label. An oscillation regression test was added in this change.
