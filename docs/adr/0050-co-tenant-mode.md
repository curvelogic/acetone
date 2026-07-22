# ADR-0050: Co-tenant mode — a graph on its own ref namespace inside a code repository

*Status: accepted — ratified by Greg at the Phase 8 / 0.3 boundary (2026-07-22) · Date: 2026-07-21 · Bead: acetone-5w6 (and children acetone-0e4, acetone-mgf, acetone-iva)*

## Context

Phase 8's goal is to let an acetone graph be a *co-tenant* of an ordinary git
repository: the graph on its own ref namespace, sharing the object store with
source code, instead of always owning a dedicated bare repo. ADR-0049 built the
seam — `GraphRefNamespace`, the single value describing where a graph's refs
live — and wired the standalone layout through it. This ADR designs the
co-tenant layout that plugs into that seam, and the one piece of plumbing the
seam could not reach: the graph's **head pointer**.

Three of the four co-tenancy assumptions (ADR-0049) are already handled once a
co-tenant `GraphRefNamespace` is constructed: branch/tag ref paths flow through
the namespace; `migrate` enumerates the namespace's prefixes (so it rewrites
only the graph's refs — code refs survive); and `gc` seeds reachability from
`references().all()`, i.e. **all** refs including code, so objects reachable
from code history already survive `gc` — the safe behaviour we must *preserve*,
never narrow. The remaining assumption is `HEAD`: today the graph drives the one
shared git `HEAD` (`read_head`/`set_head`/`head_commit_id` all bind it). In a
code repo, `HEAD` belongs to the user's source checkout; the graph needs its own
current-branch pointer.

## Decision

**Add a co-tenant layout to `GraphRefNamespace` and generalise the store's head
plumbing to a named pointer ref, so the layout — not the code path — decides
where a graph's refs and current-branch pointer live.**

### The co-tenant layout

`GraphRefNamespace::co_tenant(graph)` yields:

- branches under `refs/heads/acetone/<graph>/*` — a subnamespace *of*
  `refs/heads`, so it is proxy-safe (proxies 403 non-standard *top-level*
  namespaces but allow anything under `refs/heads`;
  `docs/notes/operational-constraints.md`), while the `acetone/` prefix keeps
  graph branches distinct from the user's code branches;
- tags under `refs/tags/acetone/<graph>/*`;
- the graph's head pointer at `refs/acetone/<graph>/HEAD` — a **local-only**
  symref (like every `refs/acetone/*`, never transferred), tracking which graph
  branch is current. The user's git `HEAD` is untouched and stays on their code.

The layout is parameterised by a graph name so multiple graphs *could* co-habit
one repository; 0.3 ships a single default graph and does not foreclose more.
Standalone is unchanged: `refs/heads/*`, `refs/tags/*`, git `HEAD`.

### The head-pointer generalisation (acetone-0e4)

`GraphRefNamespace` gains a `head_ref` — `"HEAD"` for standalone,
`refs/acetone/<graph>/HEAD` for co-tenant — and the store's head plumbing takes
the pointer name as a parameter:

- `RefStore::read_head(pointer)` — the symbolic target of `pointer` (the current
  branch ref), `None` if detached;
- `RefStore::set_head(pointer, target)` — point `pointer` (a symref) at branch
  ref `target`;
- `GitStore::head_commit_id(pointer)` — peel `pointer` to its commit.

`Repository` always calls these with `self.namespace.head_ref()`, so there is
one caller path; the store keeps the git-`HEAD` fast path (`repo.head()`) for
`pointer == "HEAD"` so **standalone is byte-identical**, and uses a generic
`try_find_reference`-based path for any other pointer. Changing these
`acetone-store` signatures is a deliberate 0.3 break; the ADR-0046-frozen
surface — the curated `acetone-core` façade and the `acetone-cypher` snapshot —
does not expand `RefStore`'s methods, so the public-API freeze job is unaffected
and no snapshot re-bless is required (verified). 0.3 permits deliberate breaking
changes regardless.

### Mode selection (acetone-mgf)

Co-tenant mode is entered through a distinct entry point,
`Repository::init_co_tenant(path, graph, options)`, which adds a graph to an
**existing** repository (`GitStore::open_discovering`) rather than creating a
fresh one like standalone `init` — co-tenancy *is* "add acetone to a repo that
already holds code", leaving the code's `refs/heads/*` and git `HEAD` untouched.
`open` detects the mode and constructs `co_tenant(graph)` vs `standalone()`.

Two mechanics differ from this ADR's first sketch, forced by how the store
works:

- **The mode marker is a *direct* ref, not the head symref.** `RefStore::list_refs`
  skips symbolic refs, so the co-tenant head symref (`refs/acetone/<graph>/HEAD`)
  cannot be *discovered* at `open`. A direct marker ref
  `refs/acetone/graphs/<graph>` (pointing at a filler empty blob) records each
  hosted graph; `open` enumerates `refs/acetone/graphs/` — none ⇒ standalone,
  one ⇒ that co-tenant graph, several ⇒ an error (multi-graph selection is
  deferred). The head symref still carries the *current-branch pointer*; the
  direct marker carries the *discoverable mode signal*. The graph name is
  validated at `init_co_tenant` (a single well-formed ref component), with the
  store door as the final backstop.

- **Co-tenant ref writes need an injected committer.** Bare acetone repositories
  never log ref updates, but the user's non-bare repository has
  `core.logAllRefUpdates` on, so every acetone ref move writes a reflog — which
  the isolated (no-config) store cannot stamp, failing `MissingCommitter`. The
  store's isolated open now injects acetone's own fixed identity
  (`committer.name`/`committer.email`) as a config override — identity strings
  only, no programs or paths, so the reduced-trust posture (ADR-0034) is intact
  — giving co-tenant ref moves a consistent, auditable reflog.

Standalone remains the default, so `acetone init` in a fresh directory is
unchanged and `git clone` still shows the graph on `main`.

### gc / migrate proof (acetone-iva)

No reachability change: `gc` keeps **all** refs as roots (code objects survive —
that is exit criterion 2's gc half, already true and now proven by a property
test). `migrate` scopes to the namespace's prefixes (code refs survive — exit
criterion 2's migrate half, delivered by the ADR-0049 seam; `acetone-ejj`
hardens it). The co-tenant main-worktree / linked-worktree `gc` case is handled
and tested.

## Consequences

- **Head plumbing is parameterised, standalone byte-identical.** The
  `acetone-store` surface changes deliberately (two `RefStore` method signatures
  + one `GitStore` inherent); the ADR-0046-guarded `acetone-core`/`acetone-cypher`
  snapshots are unaffected, so no re-bless is needed. The `"HEAD"` fast path
  guarantees no observable change for existing repositories.
- **A graph lives inside a code repo without perturbing it** — code branches
  under `refs/heads/*`, graph branches under `refs/heads/acetone/<graph>/*`, the
  user's `HEAD` on their code, the graph's current branch on its private
  pointer.
- **`refs/acetone/<graph>/HEAD` is local-only**, consistent with all
  `refs/acetone/*`: each clone chooses its own current graph branch, like git's
  `HEAD` is not itself pushed.
- **Exit criteria:** (1) coexistence — delivered by mode selection + the layout,
  tested in `acetone-mgf`; (2) `migrate`/`gc` touch only graph refs / preserve
  code — proven in `acetone-iva`; (3) the live `format_version` bump is
  `acetone-5yr` (read-old-write-new, ADR-0048), separate.
- **Foreclosed:** nothing in standalone. For co-tenant, the single-`HEAD`
  assumption is dropped in favour of a private pointer — the intended change.
- **Revisit if:** multi-graph-per-repo becomes a real requirement (the layout is
  parameterised for it, but detection/CLI ergonomics for selecting among several
  graphs are deferred), or if proxy support for top-level namespaces matures
  enough to move the head pointer/history to the cleaner Dolt-aligned
  `refs/acetone/*` top-level layout (`acetone-5w6` notes).

No format impact: ref plumbing only, no `format_version` change.
