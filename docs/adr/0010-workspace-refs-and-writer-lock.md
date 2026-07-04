# ADR-0010: Workspace refs, writer lock and Phase 1 plumbing scope

*Status: accepted (agent decision, flagged for phase-boundary review) · Date: 2026-07-04 · Bead: acetone-63m.5 · PR: pending*

## Context

Spec §3.5 and §4 fix the concepts — workspace manifests under
`refs/acetone/workspaces/<name>`, a single writer enforced by a lock
file, commits as git commits — but leave operational details open:
what the workspace ref points at, how the lock behaves after a crash,
what represents the checked-out ref, and how much graph semantics the
Phase 1 mutation path carries.

## Decision

**The workspace ref points at the manifest blob.** `refs/acetone/
workspaces/<name>` holds the chunk address of the workspace's manifest;
advancing the workspace is one compare-and-swap ref update (atomic,
crash-safe, lost races are typed errors). The namespace is local-only
and never pushed — transferable state lives in `refs/heads`/`refs/tags`
(operational constraint: proxies may reject custom namespaces).
Documented consequence: git cannot parse manifests, so the chunks a
workspace manifest references are **not** git-reachable from the blob
ref; an uncommitted workspace does not survive a foreign `git gc`.
`acetone gc` must protect workspace chunk sets when it arrives; until
then the guidance is commit before any external gc.

**Writer lock: exclusive-create file, no automatic stale-breaking.**
`<common git dir>/acetone-writer.lock`, created `O_CREAT|O_EXCL`,
holding pid and acquisition time, removed on drop, held for the life of
a `Transaction` (unlike the store's millisecond-scale
`acetone-refs.lock`). If the holder died, the next writer gets a typed
error naming the holder and the file to delete. v0.1 deliberately does
not auto-break stale locks: pid-liveness checks need platform code (or
an `unsafe`/new-dependency cost) and false positives corrupt the one
guarantee the lock exists to give. CAS on the workspace ref remains the
correctness backstop regardless; the lock is the politeness layer.

**The checked-out ref is real git HEAD.** Acetone repositories are bare
git repositories acetone owns outright (`GitStore::create`), so HEAD is
acetone's to use: `checkout` repoints HEAD symbolically, clones see the
right default branch, `git log` works natively. `RefStore` grows
`read_head`/`set_head`/`list_refs` (the last also serves fsck's
reachability walk). Phase 1 checkout targets branches only; arbitrary
commits are readable via pinned snapshots without moving HEAD.

**Phase 1 mutations are raw map plumbing.** `Transaction` stages
put/delete of nodes, edges and schema entries with no schema
validation, no constraint checks, no index maintenance (the `indexes`
map rides along unchanged), and node deletion does not cascade to
edges. Two things are enforced by construction even at this layer:
`edges_rev` updates land in the same atomic save as `edges_fwd`
(spec §3.3), and committing anchors the complete chunk set of every map
root, verified in tests against a real `git gc --prune=now`. Graph
semantics land in later beads on top of this substrate.

**Repository is concrete over `GitStore`.** No store generics until a
second store exists; the git backend is the v0.1 reference
implementation (Gate A) and speculative generality would only blur the
trait boundary that already exists at the store layer.

## Consequences

- Dolt-style WORKING state survives process exit; two writers cannot
  silently lose updates (CAS), and the second writer of a live pair is
  refused politely (lock).
- A crashed writer leaves a lock file requiring manual deletion — the
  error message carries the instruction; revisit if it bites in
  practice.
- fsck (acetone-63m.7) builds on `list_refs` + `Snapshot` accessors;
  the CLI (acetone-63m.6) is a thin client over `Repository`.
- The gc hazard for uncommitted workspaces is a known, documented gap
  owned by the future gc work (acetone-63m.13 and Phase 6 hardening).
