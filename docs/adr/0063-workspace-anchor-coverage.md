# ADR-0063: Anchor completeness for workspaces, and the one place coverage is borrowed

- Status: accepted; **ratified by Greg at the Phase 9 boundary, 2026-07-31**
- Date: 2026-07-26
- Bead: acetone-2ck.11 (PR #217); follows ADR-0015 (workspace anchoring), ADR-0014 (per-worktree workspace refs), ADR-0044 (durability anchors), acetone-5a8 (commit-side anchor completeness)

## Context

A version's chunks survive a foreign `git gc` only because something in the
git object graph references them: for a commit, its `.acetone/chunks/` anchor
tree; for a workspace, the (huo) workspace tree's anchor tree. `fsck` gained
commit-side anchor completeness in acetone-5a8 — the *clean-now-gone-later*
class, where a version verifies clean today and loses chunks to a later gc.

Extending the same check to workspaces exposed a shape with no owner. Before
ADR-0014, the workspace lived in a shared `refs/acetone/workspaces/*` ref;
after it, each worktree has its own `refs/worktree/acetone/workspace`. A
repository that predates ADR-0014 still carries the shared ref, and **nothing
in acetone ever writes, upgrades, or deletes it** — reads are shadowed by the
per-worktree ref. Those legacy refs are always bare manifest blobs, so they
anchor nothing.

Reporting them as exposed on that basis alone is wrong twice over: the claim
"these chunks would not survive a foreign git gc" is usually **false** (the
live worktree's tree anchors the same chunks), and the remedy is
**unfulfillable** (no acetone write can upgrade a ref no writer touches). A
repository in that shape would fail `acetone fsck` forever.

The first fix attempt — treat a chunk as safe if *any* workspace or anchor
tree names it — was worse, and was caught in review before merge. It is
unsound because "anchored somewhere" is not durable:

- A live workspace's own anchoring bug hides behind a peer worktree at the
  same state; both lose the chunks together, and the check exists precisely
  to catch acetone's own writers.
- ADR-0044 durability anchors are swept by `acetone gc` once their worktree
  is gone, so a chunk kept alive only by a stale anchor dies at the next gc.
  The reviewer drove exactly that sequence to a real `missing chunk` error:
  fsck clean → `acetone gc` → `git gc` → data gone.

## Decision

**Self-anchoring stays mandatory for every live version.** Commits must be
self-complete (their anchor tree travels with them), and so must every live
workspace: `refs/worktree/acetone/workspace` and each ADR-0044 anchor are
checked against their own anchor trees alone.

**Coverage is borrowed for exactly one shape**: a superseded legacy shared
`refs/acetone/workspaces/*` ref, which no writer can upgrade. Its chunks are
considered safe when a **live** workspace anchors them. Two sources qualify:

- **this worktree's own workspace tree** (`refs/worktree/acetone/workspace`),
  whose ref lives in the common directory and is therefore enumerated by a
  foreign `git gc`. On a pre-ADR-0014 repository — which by construction
  predates linked worktrees — this is usually the *only* source, so omitting
  it makes the "would not survive a foreign git gc" claim false on precisely
  the shape the message exists for (measured: fsck said 3 chunks doomed where
  `git gc --prune=now` pruned 1);
- **live linked-worktree anchors** (ADR-0044), where live means the anchor's
  `worktrees/<id>` directory still exists — the same staleness test `gc` uses
  before sweeping an anchor.

Coverage is computed only when a legacy ref is present, and only when fsck can
see the whole picture from the common directory (`git_dir() == common_dir()`),
since a linked worktree's private refs are not enumerable from elsewhere.

When such a ref *is* genuinely exposed, the finding names the shape and a
remedy that works: save from the live worktree to carry the state over, then
`git update-ref -d <ref>`.

## Consequences

- fsck keeps the clean-now-gone-later guarantee for everything acetone
  writes: an anchoring bug in a live workspace is reported, never masked.
- A pre-ADR-0014 repository is not permanently red, and the one message that
  cannot be actioned by an acetone command says so and gives the git one.
- The borrowed-coverage exception is narrow, testable and self-limiting: it
  disappears with the last legacy shared ref. Tests pin every behaviour: an
  exposed legacy ref errors; a stale anchor does not cover it; a live linked
  anchor does; this worktree's own workspace tree does (the ordinary legacy
  shape, with no linked worktrees at all); and the narrowing itself — a live
  workspace with a peer at the same state is still reported, so an anchoring
  bug in acetone's writers cannot hide.
- `acetone` still offers no ref-deletion command; the remedy quotes git
  directly rather than implying a subcommand exists.
