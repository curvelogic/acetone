# ADR-0049: `GraphRefNamespace` — one parameterised ref layout, standalone and co-tenant

*Status: accepted — pending ratification at the Phase 8 / 0.3 boundary · Date: 2026-07-21 · Bead: acetone-gns*

## Context

Phase 8 makes an acetone graph a co-tenant of an ordinary git repository: the
graph on its own ref namespace, alongside code, in one object store. Four
behavioural assumptions currently tie a graph to a repository it must *own*
(mapped exhaustively while scoping this bead):

1. **`HEAD` means "the graph's checkout".** `current_branch` reads git `HEAD`
   and filters it by the branch prefix; `set_head` (store) hard-codes the
   literal `"HEAD"`. The graph drives the one shared `HEAD` — which in a code
   repo belongs to the source checkout.
2. **`refs/heads/*` is used unqualified.** Branch name → ref path is
   `format!("{BRANCH_REF_PREFIX}{name}")` string-concatenated at five sites in
   `repo.rs` (init, create_branch, checkout_branch, resolve_commit) with the
   reverse (strip/filter) at four more across `repo.rs`, `import.rs` and the
   CLI. There is no single mapping function; a graph's branches collide with a
   user's code branches.
3. **`migrate` rewrites *all* refs.** `rewrite_history` enumerates
   `refs/heads/*` **and** `refs/tags/*` (the tag prefix a third, independently
   duplicated literal) and CAS-swings every one.
4. **`gc` walks *all* refs for reachability.** `consolidate::reachable_objects`
   seeds roots from `references().all()`.

The roadmap prescribes unifying these "behind one `GraphRefNamespace` concept"
so they flip *together*. A real open question (recorded on `acetone-5w6`) had to
be settled first: **unify or dual-mode?** Standalone acetone today puts the
graph on plain `refs/heads/main` + `HEAD` so `git clone` shows the graph on
`main` out of the box; embedded mode needs a namespaced subtree plus its own
pointer. Either collapse everything onto the embedded scheme (one code path, but
standalone loses its git-native ergonomics) or support both.

Note what is *already* co-tenant-shaped: `refs/acetone/*` and
`refs/worktree/acetone/*` (workspaces, merge-head, worktree anchors) are already
acetone-private and never transferred. Only the graph's **branches, tags and
`HEAD`** still sit in shared git-native namespaces — those three are the whole
of the conflict.

## Decision

**Introduce `GraphRefNamespace`: a value that maps a graph's logical refs
(branch and tag short names, and — as the type grows — its head pointer) to
physical git ref paths. A `Repository` holds one, constructed once at
`init`/`open`. All ref-path construction goes through it.**

Resolve the fork as **one parameterised code path, two layouts** — not two
divergent code paths:

- `GraphRefNamespace::standalone()` — branches `refs/heads/*`, tags
  `refs/tags/*`, head pointer git `HEAD`. Preserves the out-of-box
  clone-shows-graph-on-`main` ergonomics. This is the **only** layout a repo
  gets today and the default forever.
- A **co-tenant** layout — branches under `refs/heads/acetone/<graph>/*` (a
  proxy-safe subnamespace of `refs/heads`, per
  `docs/notes/operational-constraints.md`), tags under `refs/tags/acetone/*`,
  and the graph's own head pointer as a private symref (e.g.
  `refs/acetone/HEAD`) so the shared `HEAD` stays with the code checkout. This
  layout, its detection/marker, and the store plumbing it needs are **added by
  `acetone-5w6`**, constructing a different `GraphRefNamespace` at `open`. The
  code paths do not branch on mode; only the namespace value differs.

### What this bead (`gns`) delivers, and what it defers

`gns` is the **behaviour-preserving seam**. It ships the type and the standalone
layout, and routes every branch/tag ref-path site through it. Because only the
standalone layout is ever constructed, behaviour is byte-identical: all existing
store/graph/CLI tests and the pinned chunk goldens pass unchanged. It is a pure
refactor that concentrates the ref-path vocabulary in one place.

Deliberately **out of scope** here, each plugging into the same type later:

- **Assumption 1 (head pointer).** Generalising the store to read/set/peel a
  *named* symref rather than only git `HEAD` is co-tenant-specific and touches
  the frozen `acetone-store` surface (ADR-0046). Deferred to `acetone-5w6`; the
  standalone head stays git `HEAD`, unchanged.
- **Assumption 3 (migrate scoping).** `rewrite_history` keeps enumerating both
  prefixes exactly as today; `gns` only sources them from the repository's
  namespace (`repo.namespace()`, standalone today) instead of a hard-coded
  branch const and a duplicated tag literal. Behaviour is unchanged in
  standalone; scoping `migrate` to *only* the graph's refs — plus atomic ref
  swings and annotated-tag rewriting — is `acetone-ejj`.
- **Assumption 4 (gc scoping).** `gc` still seeds from all refs (which is
  already safe — code refs remain roots, so code objects survive). Scoping gc to
  the graph's refs while proving code-only objects survive is `acetone-5w6`
  (exit criterion 2).

Keeping the head plumbing and the migrate/gc scoping out means `gns` changes no
observable behaviour and touches no frozen store API, so its review is about one
question only: *is the ref-path mapping centralised faithfully?*

## Consequences

- **New surface:** `GraphRefNamespace` in `acetone-graph` with
  `standalone()` and `branch_ref`/`branch_name`/`tag_ref`/`tag_name` plus the
  `branch_prefix`/`tag_prefix` accessors used for list/scan scoping;
  `Repository::namespace(&self)`. If the `acetone-core` façade or a
  `public-api.txt` snapshot moves, it is re-blessed deliberately
  (`scripts/bless-public-api.sh`) — 0.3 permits breaking changes.
- **The five branch-ref concatenation sites and three tag-prefix literals
  collapse to one definition.** A future layout change happens in one value, not
  nine edits — which is exactly what makes `5w6`/`ejj` tractable.
- **The type will grow.** `5w6` adds a head-pointer field and the co-tenant
  constructor; `ejj` threads the real namespace into `migrate`. The standalone
  accessors shipped here are their foundation.
- **Foreclosed:** nothing. Standalone behaviour is preserved bit-for-bit; the
  co-tenant path is opened, not taken.
- **Revisit if:** the co-tenant layout (in `5w6`) discovers a ref concern the
  path-only type cannot express (e.g. a per-graph config that must travel with
  the refs), in which case the type absorbs it rather than a second code path
  reappearing.

No format impact: this is ref plumbing only, no `format_version` change.
