# Maintenance and migration

A version-controlled graph accumulates: commits, chunks, superseded workspace
states, old formats. This chapter covers the three commands that keep a
repository healthy — `fsck` (verify), `gc` (consolidate) and `migrate`
(rewrite) — plus the writer lock that guards it all, and how acetone crosses
a format-version boundary. It runs against the
[asset registry](../getting-started/asset-registry.md) as the previous
chapters left it.

## `acetone fsck`: verify everything

`fsck` is the integrity auditor. For **every version reachable from
workspaces, branches and tags** — annotated tags are peeled and symbolic refs
followed, so nothing ref-shaped escapes the sweep — it verifies:

- **manifest decode** — each commit's manifest parses at its recorded format
  version;
- **chunk reachability** — every chunk each prolly tree references is present
  in the object store;
- **prolly-tree structure** — trees are well-formed, and a
  history-independence spot-check confirms each map is in canonical form (a
  non-canonical map is an error: it would break the identical-content ⇒
  identical-hash invariant);
- **graph consistency** — no dangling edges (you saw this catch a real one
  mid-merge in the [history chapter](history-branch-merge.md), where it
  named the exact edge the commit error would not);
- **derived-data consistency** — edge-map symmetry (`edges_rev` mirrors
  `edges_fwd`) and index agreement with the nodes map;
- **schema constraints** — nodes breaching a declared `--require` or
  `--unique` constraint are named, per version.

Findings come in two severities. **Errors** mean integrity is broken and the
exit status is non-zero. **Advisories** cover the derived-data and
constraint checks — index divergence is repairable in place by
[`acetone reindex`](schema-and-indexes.md), and a constraint breach (possible
in repositories written before writes, imports and declarations all enforced
constraints) is a data problem to fix with ordinary writes, not a corruption
of record. A healthy repository is one word:

```console
$ acetone fsck
fsck: clean
```

The ref sweep really does mean every ref. Add an **annotated** tag — a ref
pointing at a tag object, not a commit — and fsck peels it and audits the
version underneath without complaint:

```console
$ git tag -a audit-2026-07 -m "quarterly audit point"
$ acetone fsck
fsck: clean
```

(`query --at` peels annotated tags the same way — but, as you will see
below, `migrate` does not yet.)

Run fsck whenever you would run `git fsck`: after an interrupted operation,
after moving or copying a repository, before relying on a backup, or simply
on suspicion. It only reads; it is always safe.

## `acetone gc`: consolidate the object store

Every commit writes its chunks as loose git objects — cheap and clean, but
git's own packing heuristics do not understand content-defined chunks, so
left to git the store never deltas well. `acetone gc` is acetone's **own**
consolidation: it rewrites the reachable object set into one self-contained
packfile, delta-compressing each chunk against the predecessor acetone
recorded at write time, then prunes the loose copies it has verified are in
the pack:

```console
$ git count-objects -v | head -4
count: 356
size: 1424
in-pack: 0
packs: 0
$ acetone gc
gc: packed 118 object(s) (14 delta, 104 whole) into 14976 bytes; pruned 118 loose object(s), 0 superseded pack(s)
$ git count-objects -v | head -4
count: 238
size: 952
in-pack: 118
packs: 1
```

What to know about its behaviour:

- **Representation-only, guaranteed.** Consolidation never changes any
  object's bytes, so every object ID — and every prolly root above it — is
  preserved exactly. A stored copy is deleted only after the object is
  confirmed present in the freshly written pack.
- **Unreachable objects are left alone.** The 238 objects remaining above are
  mostly superseded intermediate workspace states no longer reachable from
  any ref. gc does not prune what it did not pack — unreferenced loose
  objects are harmless, and leaving them is part of crash safety.
- **Idempotent.** A second run repacks to the same result and prunes nothing:

  ```console
  $ acetone gc
  gc: packed 118 object(s) (14 delta, 104 whole) into 14976 bytes; pruned 0 loose object(s), 0 superseded pack(s)
  ```

- **Co-tenant safe.** In a repository shared with code (an acetone graph
  living alongside a codebase), objects reachable from non-graph refs form a
  prune guard — gc never disturbs them.
- **Stock `git gc` is safe but lossy.** Running git's own gc or repack on an
  acetone repository corrupts nothing, but discards the hand-chosen deltas
  (git's heuristics never pair a content-addressed chunk with its
  predecessor). Re-running `acetone gc` restores them. Prefer acetone's.

When to run it: after churn — a large import, a long editing session, a
history rewrite — or periodically. Between times, loose objects cost disk,
never correctness.

## The writer lock

acetone is single-writer per repository: every ref update is serialised
through a lock file, `acetone-refs.lock`, in the repository's (common) git
directory. It is held for the duration of one update and removed
automatically — in normal use you will never see it. Concurrent **readers**
are never blocked: every reader is pinned to an immutable snapshot, so
queries proceed while writes happen.

If a process dies while holding the lock — SIGKILL, power loss — the stale
file remains, and the next write backs off for about five seconds and then
fails rather than hang or corrupt anything. Simulating exactly that
(paths shortened below):

```console
$ touch acetone-refs.lock
$ acetone branch audit-fixes
error: creating branch "audit-fixes": git backend error while locking refs for compare-and-swap: The lock for resource '/…/registry/acetone-refs' could not be obtained after 5.00s after 26 attempt(s). The lockfile at '/…/registry/acetone-refs.lock' might need manual deletion.: The lock for resource '/…/registry/acetone-refs' could not be obtained after 5.00s after 26 attempt(s). The lockfile at '/…/registry/acetone-refs.lock' might need manual deletion.
```

Recovery is manual and safe: **first confirm no acetone process is running
against the repository**, then delete the file and retry:

```console
$ rm acetone-refs.lock
$ acetone branch audit-fixes
created branch "audit-fixes" at 9a85a178c76bf71268ece2e6e327ed470f5af9e2
```

(For a repository checked out through git worktrees, the lock lives in the
common/main git dir, shared by all worktrees — like the refs it guards.
And while we are here: deleting a branch is a git operation,
`git branch -d audit-fixes` — acetone's `branch` only lists and creates.)

One subtlety if you test this yourself: the lock serialises ref
**updates**, and an operation that changes nothing performs no ref update.
Workspace writes are content-addressed, so re-running a write whose values
are already present — say, a `SET` that assigns a property the value it
already has — reproduces the identical workspace state, writes no ref, and
succeeds instantly even under a stale lock. That is the no-op fast path,
not a hole in the lock: any write that actually changes the graph fails
with the backoff error above, exactly like `branch` and `commit`.

## Format versions and `acetone migrate`

Every manifest records a `format_version` — the version of acetone's key and
value encodings, chunker format and manifest schema. It is read first on
every decode; the current format is **version 1**, frozen for the 0.x
releases. One day a release will introduce version 2, and how a repository
crosses that boundary was settled deliberately (ADR-0048). There are two
strategies, and the default is the non-destructive one.

### The default: read old, write new

An acetone build retains a decoder for every format version it has ever
shipped and dispatches on each manifest's recorded version. Old commits stay
readable through their era's decoder; new writes always use the current
format; **no existing commit is ever rewritten**. A repository holding v1
and v2 commits side by side is a normal, valid, permanent state — not a
defect awaiting cleanup. This matters most for a graph sharing its
repository with code and collaborators: adopting a new acetone build never
rewrites hashes anyone has fetched, so there is nothing to force-push.

The one hard edge: a repository written by a *newer* build than yours (a
version your build has no decoder for) is rejected with a clear error rather
than misread. The fix is to upgrade acetone, never the repository.

### The opt-in: rewriting history

`acetone migrate` is the deliberate alternative: a generic history-rewrite
engine that decodes every reachable commit, re-encodes it, and rebuilds the
commit graph — preserving each commit's message, author and committer
(identity and timestamp) verbatim, but necessarily producing **new hashes
for every commit**. Sharing the result means a force-push. It exists for the
standalone repository that *is* the graph — no code co-tenant, no fetched
clones to diverge — and wants a single-format history, or wants to retune
storage. It refuses a dirty or mid-merge workspace, and (a current
limitation) a repository with annotated tag refs, since it cannot yet
rewrite tag *objects* — delete or lighten them first:

```console
$ acetone migrate
error: rewriting history: ref "refs/tags/audit-2026-07" does not point at an acetone commit
$ git tag -d audit-2026-07
Deleted tag 'audit-2026-07' (was 38c6730)
```

The transform it applies today is **re-chunking**: rebuilding every map under
new chunk-size parameters (a future format-version bump plugs into the same
engine). Run with no flags it re-chunks under the repository's *current*
parameters — and here the history-independence invariant makes a quietly
spectacular guarantee: rebuilding every tree from scratch reproduces
byte-identical roots, so every commit hash comes out **unchanged**. A no-flag
migrate is therefore a safe repair pass:

```console
$ acetone migrate
migrate: rewrote 11 commit(s), updated 4 ref(s)
$ acetone log | head -1
9a85a178c76bf71268ece2e6e327ed470f5af9e2 interface inventory: schema and first entries
```

Same head hash as before. Now a real parameter change — on a **copy**, which
is the sensible way to run any history rewrite (`cp -a` of the repository
directory is a complete backup):

```console
$ cp -a registry registry-rechunk
$ cd registry-rechunk
$ acetone migrate --mask-bits 13
migrate: rewrote 11 commit(s), updated 4 ref(s)
$ acetone log
330a9966b2cf93815da22cd816848e5ab89605e9 interface inventory: schema and first entries
b09e2d14f1ee52f6583d88a287619508f8b28b56 revert: keep the team name web
7a392d60a6caf06073b93ccceb00129203959f64 team web renamed to webshop
7c15214a295698d8788484845d3661eb17edc97b merge bump-identity
49bb789222d097f8c88979abbda1c36c7eaa21f3 retire db2
6a46e4b47bf507dfcd5b8e60d8c78935c9a85c13 identity patch release 2.4.2
ff95cd17b174444d217ca25c8da5402ab954fd45 merge decommission-app1
e88f41729f2dd46991ac9b53bad312d4680d608c postgres upgraded to 16.4
64fd4bb52a6500630ded737578fc7e28e108b298 asset registry: initial inventory
$ acetone fsck
fsck: clean
```

Every commit re-encoded, every hash new, messages and topology intact, and
fsck vouches for the result. The rewrite is deterministic — re-running the
same migrate produces the same hashes, so an interrupted run is completed by
running it again. The superseded old commit graph becomes unreachable and is
reclaimed by the next `acetone gc`.

### Which do you need?

Almost always: **neither, yet**. The registry is at format 1 like every
current repository. When a format bump arrives, the default path asks
nothing of you — old commits keep reading, new commits use the new format.
Reach for `migrate` only when you positively want a rewritten history:
retuned chunking, or a standalone repository you prefer format-uniform, and
you accept new hashes as the price.

---

That is the working life of a repository: `fsck` when in doubt, `gc` after
churn, `migrate` almost never. When something is actually *broken* — refs
gone, chunks missing, a workspace that will not load — proceed to
[Part III: the recovery runbook](../recovery/runbook.md).
