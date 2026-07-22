# ADR-0051: Co-tenant `gc` is graph-scoped — it packs only the graph's objects and leaves code storage untouched

*Status: superseded by the boundary ruling — Greg ruled **(B) graph-scoped** at the Phase 8 / 0.3 boundary (2026-07-22) and directed it be **delivered with full assurance** before co-tenancy is considered complete (not shipped as an (A) interim). (A) is the shipped starting point being replaced; (B) lands via `acetone-wao` (graph-scoped consolidation + re-proof) then `acetone-5cw` (`.keep` durability vs foreign `git gc`). · Date: 2026-07-21 (ruled 2026-07-22) · Bead: acetone-iva → acetone-wao*

## Context

Phase 8 exit criterion 2 reads: "`migrate` and `gc` provably touch only graph
refs (objects reachable only from code history survive `gc`; code refs survive
`migrate`)." Writing the proof test for the `gc` half (`acetone-iva`) surfaced a
question the wording alone does not settle, raised by the PR review: *what does
"touch only graph refs" mean for `gc` when the graph co-tenants a code
repository?*

How acetone `gc` actually works (`consolidate`): it computes a reachable set
seeded from **all** refs (`references().all()`), writes that whole set into one
new pack, then `prune_loose` deletes the now-redundant **loose** copies of
exactly the objects it just packed. It never enumerates the object directory to
delete unreachable objects — pruning is gated on membership in the fresh pack.

Two consequences bear on the criterion:

- **Code objects survive** — they are reachable from code refs, so they are in
  the reachable set, packed, and remain retrievable. Exit criterion 2's "survive
  `gc`" holds. Good.
- **But `gc` reads and repacks the user's code objects** and deletes their loose
  copies. It is byte-preserving and `git fsck`-clean (identical object content,
  just loose → pack, in an acetone-authored pack), and it moves no code ref — but
  it *does* rewrite the physical storage layout of code the graph does not own.

So there are two coherent readings of "touch only graph refs":

- **(A) Repo-global repack.** `gc` is a whole-repository maintenance operation,
  exactly like `git gc`: it may repack any reachable object, and the co-tenancy
  guarantee is "no code **ref** is moved and every code object stays retrievable
  and `fsck`-clean." This is what ships today.
- **(B) Graph-scoped repack.** `gc` must leave objects it does not own physically
  untouched — pack and prune only objects reachable from the *graph's* refs,
  never the user's code objects.

## Decision

**(B) graph-scoped is the adopted semantics.** Greg ruled at the Phase 8 / 0.3
boundary (2026-07-22): `acetone gc` must leave objects it does not own physically
untouched — pack and prune only objects reachable from the *graph's* refs, never
the user's code objects. It is to be **delivered with full assurance** (not
shipped as an (A) interim).

(A) — the repo-global repack that shipped in `acetone-iva` — was the honest
interim: it kept every code object retrievable and `fsck`-clean and moved no code
ref, so exit criterion 2's "survive `gc`" held. But it *repacked* the user's code
objects into an acetone-authored pack, rewriting the physical storage of code the
graph does not own. Two things make (B) the right end state:

- **Ownership.** "Alongside code" means acetone disturbs nothing it was not handed.
  Repacking a co-tenant's code objects — even byte-preservingly — is a surprise a
  guest should not spring.
- **Durability against foreign `git gc`.** A co-tenant repo's owner runs `git gc`
  (and git's *automatic* `gc.auto`) for their code, routinely and often silently.
  Under (A) acetone's pack contains code objects, so protecting it with a `.keep`
  (ADR-0053, `acetone-5cw`) would freeze the user's code-object packing under
  acetone — invasive. Under (B) acetone's pack is graph-only, so it can be
  `.keep`-protected to preserve acetone's content-aware deltas **without** touching
  how the user's code is packed. (B) is the reading that makes the durability fix
  clean.

### How (B) is realised (`acetone-wao`)

`consolidate` seeds its *reachable-to-pack* set from the graph's refs only
(via the repository's `GraphRefNamespace`), not `references().all()`. It
additionally computes the set reachable from **non-graph** refs as an explicit
**prune-guard**: a loose object in that guard set is never pruned, even if it is
also graph-reachable (a shared object stays as git left it). Because a
standalone repo's graph refs *are* all of `refs/heads/*` + `refs/tags/*` + `HEAD`,
the graph-scoped set equals the all-refs set there, so **standalone consolidation
is byte-identical** to today; only co-tenant mode narrows. The `packed == present`
tripwire is retained against the graph-reachable `present`. The `acetone-iva`
proof is rewritten: a code-only object **survives `gc` and is *not* in acetone's
pack** (its loose/packed representation is left as git had it), which is the
discriminating property (A) could not satisfy.

## Consequences

- **Exit criterion 2's `gc` half is met under (B)** once `acetone-wao` lands:
  `acetone gc` provably touches only graph objects, and a code-only object is
  preserved *and* left in git's own storage (not consolidated into acetone's
  pack). The `acetone-iva` proof is rewritten to assert that discriminating
  property. Until `wao` merges, (A) is what the code does — the honest interim.
- **`acetone gc` no longer repacks code objects.** In a co-tenant repository it
  packs and prunes-loose only the graph's objects; the user's code objects are
  left exactly as git arranged them (loose or in git's packs), with an explicit
  prune-guard so nothing reachable from a non-graph ref is ever pruned.
- **Standalone is unchanged.** A standalone repo's graph refs are all of
  `refs/heads/*` + `refs/tags/*` + `HEAD`, so the graph-scoped reachable set
  equals the all-refs set and consolidation is byte-identical to today.
- **This unblocks the durability fix.** With a graph-only pack, `acetone-5cw`
  (ADR-0053) can mark it `.keep` so a foreign `git gc`/`git repack` (and
  `gc.auto`) leaves acetone's content-aware deltas intact — without freezing the
  user's code-object packing.
- **Cost:** `consolidate` walks non-graph refs too (for the prune-guard), so it
  is not blind to code history — but it never *repacks* it, which is the
  expensive and intrusive part. The efficiency and ownership wins are in what it
  writes and deletes, not in what it reads.

No format impact: `gc` changes object *storage*, never object *content* or
`format_version`.
