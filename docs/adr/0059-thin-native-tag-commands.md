# ADR-0059: Thin native tag commands, not git pass-through

*Status: accepted — approach chosen by Greg in session discussion (2026-07-24) · Date: 2026-07-24 · Bead: acetone-ujsk*

## Context

The 0.3.1 phase shipped tag *reading* — `resolve_commit`/`--at` peel annotated
tags and expand short names within the graph's namespace, `fsck` peels and
verifies them, `migrate` rewrites them — but no tag *creation* surface. In
co-tenant mode (ADR-0050) the physical layout leaks: making a tag that `--at
v1` resolves by short name requires hand-typing
`git tag acetone/<graph>/v1 <commit>`, while a natural `git tag v1` lands in
the *code* repository's namespace, where the graph's `gc` does not own it and
`migrate` does not rewrite it. (Reachability is not the gap — a full
`refs/tags/…` path resolves regardless; the gap is short-name ergonomics plus
gc/migrate management, both of which follow from namespace placement.)

Four options were weighed:

1. **Own `acetone tag` commands** — symmetric with `acetone branch` (#190),
   which already owns ref porcelain for the graph namespace; risks porcelain
   creep (`-f`, `-v`, signing, editor flows…).
2. **Docs-only** — teach the manual the raw namespaced path. Zero code, but
   makes the physical layout part of the user interface, exactly what
   ADR-0050's namespace abstraction exists to hide.
3. **Resolver-side fallback** — co-tenant `--at` also tries plain
   `refs/tags/<name>`. Rejected as wrong, not just inferior: it leaks the code
   repo's tags into the graph's refspec space and still leaves the tag foreign
   to `gc` and `migrate`.
4. **Native but explicitly thin** — option 1 with the boundary declared up
   front: create/list/delete at the namespaced path and nothing more.

**Intermediated pass-through to the git binary** was also considered — shell
out to `git tag` with the short name rewritten to the namespaced path. It
would buy porcelain fidelity (signing, `-v`, `$EDITOR`, the user's real
identity) but was rejected: it breaks the embedded-library story (the store
is deliberately in-process via gitoxide, and a CLI-only shell-out gives
library consumers nothing); it bypasses the store-level lock that serialises
acetone's compare-and-swap ref writes (an external `git` process races in
exactly the window that lock exists to close); and the namespace translation
is the hard part anyway, so wrapping the binary saves little over the native
version, which reuses the existing tag module (`read_tag`/`peel_tag`/
`rewrite_tag` — creation is the missing quarter).

## Decision

**Ship native `acetone tag` commands, scoped thin, as sugar over the
namespaced path** — mirroring the `acetone branch` UX:

- `acetone tag` lists the graph's tags (short names);
- `acetone tag NAME [REFSPEC] [-m MSG]` creates an **annotated** tag at
  `namespace.tag_ref(NAME)` (message defaults to the tag name);
- `acetone tag -d NAME` deletes (ref plumbing only, like branch deletion).

Everything else — signing, verification, message editing, forced moves — is
git's job, at the namespaced path, and the manual documents the equivalence
rather than the commands growing to cover it. Declining to *create* signed
tags is coherent with the existing line that `migrate` refuses to *rewrite*
them: acetone does not create what acetone cannot manage.

The tagger identity is the same neutral placeholder commits use
(`acetone <acetone@acetone.invalid>`): the store opens gix isolated
(ADR-0034), so ambient git config is not read. Sourcing real identity from
config is a separate, uniform commits-and-tags decision (acetone-gid), not
smuggled in here.

## Consequences

- Co-tenant users get short-name tagging that `--at`, `gc` and `migrate` all
  manage, without knowing the physical layout; standalone mode is unchanged
  (`git tag v1` already lands in the graph's namespace there).
- Library consumers get the same capability through
  `Repository::{tags, create_tag, delete_tag}` — no git binary required.
- The thin boundary is the declared defence against porcelain creep: future
  "add `-f`/`-v`/signing" requests are answered by the manual's git
  equivalence note, or by revisiting this ADR deliberately.
- One new store write path (`GitStore::create_tag`, a fresh annotated tag
  object); no invariant is touched — the load-bearing bit is namespace
  placement, which is what the commands exist to get right.
