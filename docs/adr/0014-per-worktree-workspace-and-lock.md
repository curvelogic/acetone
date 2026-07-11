# ADR-0014: Per-worktree workspace ref and writer lock

*Status: accepted — ratified by Greg at the pre-0.1 boundary review (2026-07-11); originally an agent decision flagged for phase-boundary review · Date: 2026-07-06 · Bead: acetone-rjf · Amends: ADR-0010*

## Context

ADR-0010 put the workspace manifest under a single shared ref
`refs/acetone/workspaces/default` and the single-writer lock in the git
**common** directory. That makes git worktrees second-class: two worktrees
of one clone — the git-native way to have two branches checked out at once
— would share one workspace ref and one writer lock, so a writer in
worktree A on `feature-1` and a writer in worktree B on `feature-2` collide
on both, even though git keeps their HEAD, index and `index.lock`
per-worktree. Acetone's analogue of the index is the workspace manifest, so
the workspace ref and the writer lock should be per-worktree too.

Greg raised this at the Phase 1 boundary: settle the workspace/lock model
before the Phase 3 write path (acetone-mex.2) builds `save`/`commit` on it.

## Investigation

The implementation risk flagged on the bead was whether gix (0.85,
default-features off) resolves git's per-worktree ref namespace
`refs/worktree/*` the way git does. A throwaway probe (git CLI to build a
linked worktree, gix reduced-trust `open_opts` on each git dir) settled it:

- `repo.path()` is the per-worktree git dir (`<common>/worktrees/<id>`);
  `repo.common_dir()` is shared. They differ for a linked worktree and
  coincide for the main worktree.
- A ref written under `refs/worktree/` in the worktree is found from that
  worktree and **absent** from the main worktree — gix resolves the whole
  `refs/worktree/` subtree per-worktree, and stores it in the worktree's
  private dir, not the common dir.
- Ordinary refs (`refs/acetone/workspaces/*`) stay shared across worktrees.

So the git-native namespace works; no worktree-id-qualified fallback is
needed.

## Decision

**Writer lock moves to the per-worktree git dir.** `WriteLock::acquire`
now takes the git dir (`GitStore::git_dir()`, new, wrapping
`gix::Repository::path()`) instead of the common dir. Two worktrees on
different branches get independent `acetone-writer.lock` files and run
concurrently — the payoff. For a repository with no linked worktrees, git
dir == common dir, so behaviour is unchanged.

**The workspace ref becomes a single per-worktree ref**
`refs/worktree/acetone/workspace`. v0.1 has one workspace per checkout
(the previous `refs/acetone/workspaces/<name>` never used a name other than
`default`), so the workspace collapses to one fixed ref that git resolves
per-worktree. This keeps the change small: no per-worktree ref enumeration
is needed (fsck reads the one ref by name), sidestepping any question about
whether gix's `references().prefixed(...)` lists private per-worktree refs.

**The short-lived `acetone-refs.lock` CAS window stays common.** It only
guards gix's CAS window for milliseconds; concurrent worktrees write
*different* refs, so it never serialises them for real. Unchanged from
ADR-0010.

**Migration is a graceful fallback, not a rewrite.** Workspace refs are
local-only, never pushed, and hold disposable working state. On read, if
the per-worktree ref is absent, acetone falls back to the legacy shared
`refs/acetone/workspaces/default` — so an existing repository keeps its
uncommitted workspace across the upgrade. The first `save`/`checkout`
writes the per-worktree ref forward via CAS; the legacy ref then lingers
harmlessly (ignored, local-only, garbage-collectable). No format bump, no
history migration — chunk and manifest bytes are untouched (workspace refs
are a ref-plumbing concern, not a graph-format one).

**fsck checks the current worktree's workspace.** It reads the one
per-worktree workspace ref (with the same legacy fallback) instead of
enumerating a shared prefix. Each worktree fscks its own working state,
which is the correct per-worktree model and the shape huo (workspace chunk
anchoring) will extend.

## Consequences

- Worktrees are first-class: independent writers and independent working
  state per worktree, matching git's own per-worktree model.
- Cross-clone is unchanged and deliberately not a mutex (anti-git): clones
  diverge and merge.
- ADR-0010's workspace-as-blob target and its writer-lock *mechanism*
  (exclusive-create, no stale-breaking) are unchanged — only *where* the
  ref and the lock live moves. huo (acetone-huo) will change *what* the
  workspace ref points at (a `{manifest, chunks/}` tree) on top of this.
- The legacy-ref fallback is a one-release convenience; it can be dropped
  once no pre-rjf repositories remain (tracked with huo/Phase 6).
- `acetone-gc`/`gc.auto` handling for the per-worktree workspace is huo's
  concern; this ADR does not change the gc-durability story.
