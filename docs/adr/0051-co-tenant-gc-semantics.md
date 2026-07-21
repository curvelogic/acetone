# ADR-0051: Co-tenant `gc` is a repo-global repack — code objects are preserved, not left untouched

*Status: proposed — flagged for Greg's ruling at the Phase 8 / 0.3 boundary (exit-criterion interpretation) · Date: 2026-07-21 · Bead: acetone-iva*

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

**Ship (A) for 0.3 and record it as the interpretation of exit criterion 2's
`gc` half; flag (B) as a deferred alternative for Greg's ruling at the
boundary.**

Rationale for (A) as the default: acetone `gc` is a deliberate, user-invoked
maintenance action (not something that runs on every write), and repacking the
reachable set is precisely what `git gc` does to the same repository — so it is
neither surprising nor lossy. The co-tenancy promise that matters — *the graph
never rewrites or moves the user's code history* — is kept: no code ref moves, no
commit hash changes, every object stays retrievable. The `acetone-iva` proof
test asserts the discriminating property (a code blob is drawn into `gc`'s pack —
its loose file consolidated away yet still retrievable — which only holds if
`gc`'s reachable set includes code), so a regression to graph-scoped reachability
would fail it.

This is an **exit-criterion interpretation**, which is Greg's call at the phase
boundary. It is recorded here, and prominently in the Phase 8 report, as the
shipped reading with (B) as the alternative he may prefer.

## Consequences

- **Exit criterion 2 is met under reading (A)** and proven by `acetone-iva`. The
  phase report states the interpretation explicitly rather than leaving "touch
  only graph refs" ambiguous.
- **`gc` repacks code objects.** In a co-tenant repository, `acetone gc`
  repacks and prunes-loose the user's code objects along with the graph's —
  byte-preserving and `fsck`-clean, equivalent to `git gc`, but it is whole-repo
  maintenance, not graph-only.
- **Foreclosed for now:** the stronger (B) guarantee. Adopting it later means
  scoping `consolidate`'s reachable set (and pack) to the graph's refs while
  *still* treating code refs as roots that must not be pruned — a more intricate
  reachability split. No format impact either way.
- **Revisit if:** Greg rules for (B) at the boundary, or a co-tenant user finds
  acetone repacking their code objects undesirable (e.g. it disturbs their own
  pack tuning). Until then (A) stands, documented.

No format impact: `gc` changes object *storage*, never object *content* or
`format_version`.
