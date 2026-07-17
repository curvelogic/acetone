# ADR-0044: Anchor a linked worktree's uncommitted workspace under a common gc-rooted ref

*Status: accepted (agent decision, autonomous Phase 7 — flagged for phase-boundary review) · Date: 2026-07-16 · Bead: acetone-7tf · Amends: ADR-0015 · Relates: ADR-0014, acetone-gns*

## Context

ADR-0015 made the **main** worktree's uncommitted workspace gc-durable by
pointing the workspace ref at a *tree* whose `chunks/` subtree anchors every
chunk the manifest references, so git's reachability walk keeps them across a
foreign `git gc --prune=now`.

That guarantee did not extend to a **linked** worktree. A linked worktree's
workspace ref is `refs/worktree/acetone/workspace`, which git resolves to the
worktree-private ref store `<common>/worktrees/<id>/refs/worktree/...`. PR #131
confirmed at the pure-git level (git 2.48.1) that `git gc` run from the main
worktree does **not** enumerate another worktree's `refs/worktree/*` refs as
reachability roots — a tree *or* commit reachable only through such a ref is
pruned. So a user running stock `git gc --prune=now` from the main worktree,
with saved-but-uncommitted work in a linked worktree, could lose that work's
chunks. (Acetone's *own* gc was never exposed: it refuses while any linked
worktree exists, ADR-0014.)

## Decision

**Every time a linked worktree advances its workspace, mirror the workspace
tree into a common-store anchor ref `refs/acetone/worktree-anchors/<id>`.**

- `<id>` is the basename of the worktree's git dir — i.e. the `worktrees/<id>`
  directory name git itself assigns, a stable per-worktree key.
- The anchor lives under `refs/acetone/*`, an *ordinary* (non-`worktree`)
  namespace, so gix writes it to the **common** ref store, which git
  enumerates globally as a gc root. That is the whole mechanism: the linked
  worktree's workspace tree — and thus its chunks — is now reachable from a
  ref foreign gc always sees. (A TDD end-to-end test proves gix routes
  `refs/acetone/*` to the common dir from a linked worktree; the fix depends
  on it.)
- The main worktree needs no anchor: its `refs/worktree/acetone/workspace`
  already lives in the common dir and is gc-enumerated. `cas_workspace` writes
  an anchor **iff** `git_dir() != common_dir()`.
- The anchor merely *follows* the workspace tree, so it is **force-written**
  (`GitStore::overwrite_ref`, `PreviousValue::Any`), not compare-and-swapped.
  There is nothing to race: the sole writer for a given `<id>` is that
  worktree's writer, already serialised by the per-worktree single-writer lock
  (ADR-0014). A failed anchor write **fails the save** — durability is the
  point, so a silent partial is worse than a loud error.

**`GitStore::overwrite_ref` is an inherent method, not a `RefStore` trait
method.** The `RefStore` contract deliberately documents that it offers *no*
unconditional overwrite (writes are compare-and-swap only). Rather than weaken
that contract, the force-write is an inherent `GitStore` primitive — mirroring
the existing inherent `set_head`, which also uses `PreviousValue::Any` — and is
used only for this anchor.

**Stale anchors are pruned by acetone's own `gc`.** An anchor keyed by `<id>`
outlives its worktree if the worktree is removed. But acetone `gc` runs *only*
when no linked worktree exists (ADR-0014), so at that moment **every** anchor is
by definition stale. `gc` therefore deletes all `refs/acetone/worktree-anchors/*`
before consolidating, so their now-unreferenced chunks are reclaimed. Live-
worktree anchors are never pruned, because gc refuses to run while a worktree
is live. This is the efficiency tail; correctness is the anchor write.

**Local-only, no format change.** `refs/acetone/*` is never transferred
(operational-constraints: only `refs/heads|tags` carry transferable state), and
no manifest byte, chunk, or encoding changes — the anchor references existing
blobs. No `format_version` bump.

## Consequences

- A linked worktree's saved-but-uncommitted workspace now survives an
  aggressive foreign `git gc --prune=now` from the main worktree — the huo
  guarantee (ADR-0015) holds for **every** worktree, not only the main one. The
  previously `#[ignore]`d reproduction in `repository.rs` is now a passing test;
  a companion test covers stale-anchor pruning by `gc` after a worktree is
  removed and `git worktree prune`d.
- One extra force-write per save/merge/abort/reindex **in a linked worktree**
  only; the main worktree's path is unchanged. The anchor tree is the same
  object as the workspace tree (already built), so it costs one ref write, no
  extra object storage.
- The anchor inherits the workspace tree's per-save O(total chunks) build cost
  (ADR-0015's known latency; incremental anchoring is acetone-taf) — it adds no
  new asymptotic cost of its own.
- `refs/acetone/worktree-anchors/*` is a fourth `refs/acetone/*` co-tenant
  alongside workspaces, indexes, and merge state. Their unification into one
  namespace scheme is tracked on acetone-gns; this ADR notes the overlap but
  does not block on it.
- **Residual narrow window:** an anchor is renewed on each workspace advance,
  keyed by the *current* worktree id. If a worktree is removed and a foreign
  `git gc` runs *before* acetone `gc` prunes the stale anchor, the stale anchor
  simply keeps some now-dead chunks alive a while longer — a space leak, not a
  data-loss bug — reclaimed at the next acetone `gc`. This is the intended
  trade (durability over promptness of reclamation).
