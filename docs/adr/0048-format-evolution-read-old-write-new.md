# ADR-0048: Format evolution — read-old-write-new is the default, rewrite-migrate is opt-in

*Status: accepted — pending ratification at the Phase 8 / 0.3 boundary · Date: 2026-07-21 · Bead: acetone-fev*

## Context

Phase 8 makes an acetone graph a co-tenant of an ordinary git repository — the
graph lives on its own ref alongside code history in one object store. The
phase will also introduce `format_version = 2` (the first real bump since Gate D
froze v1 in ADR-0024). Before that bump lands we must settle *how* a repository
crosses a format boundary, because the two mechanisms we could use pull in
opposite directions.

Today there is exactly one: `acetone migrate` (ADR-0025), a generic
history-rewrite engine. It brings an old format *up to* current by decoding
every reachable commit, re-encoding its manifest, and writing a fresh commit —
which, because chunk hashes are content-addressed, **changes every commit hash**
and so requires a force-push to share. That is acceptable for a standalone
repository that *is* the graph (pre-1.0 accepts new hashes), but it is
incompatible with the co-tenancy pitch:

- A co-tenant graph shares its repository (and its collaborators) with code.
  Rewriting the graph's history rewrites the hashes every collaborator has
  fetched; a force-push then diverges their clones. "Your graph upgraded itself
  and force-pushed" is not a proposition a shared repository can accept.
- The whole promise of *alongside code* is that adopting a new acetone build
  should not perturb history that other tools and people already depend on.

The manifest format was designed for exactly this. Its top level is the stable
two-element array `[format_version, body]`: a reader reads the version *first*
and only then interprets `body` as version-`N` territory (`manifest.rs`). So a
single repository can already hold commits at several format versions
side-by-side — the object store never conflicts. The only thing stopping us
reading such a repository is policy in the decoder: `Manifest::decode` currently
rejects any `version != FORMAT_VERSION` with `UnsupportedVersion` rather than
dispatching to a reader for that version. That rejection is the deferred half of
Gate D's "`format_version` bump machinery" (ADR-0024), and `acetone-5yr` is the
bead that implements the dispatch.

We therefore have two genuine, complementary strategies and must say which is
the default.

## Decision

**Read-old-write-new is the default format-evolution path. History-rewriting
`migrate` is retained but reserved for explicit opt-in.**

*Read-old-write-new* (implemented by `acetone-5yr`): the binary retains a
decoder for every format version it has ever shipped and dispatches on the
manifest's `format_version`. Old commits are read through their era's decoder;
**new** writes always use the current format. No existing commit is rewritten,
**no hash changes**, and history shared with code or collaborators is untouched.
A repository is free to contain a mix of format versions — old commits at v1,
new commits at v2 — and that mixed state is valid and expected, not a defect to
be cleaned up. An old commit is upgraded only incidentally: if some *other*
operation rewrites it (a `migrate`, or a future rechunk), the rewrite emits
current-format bytes; otherwise it stays at its original version indefinitely,
and that is fine.

*Rewrite-migrate* (ADR-0025) remains in the product unchanged, but as the
**opt-in** tool for the standalone model: a repository that *is* the graph, has
no code co-tenant and no shared collaborators, and wants a single-format history
(or wants to reclaim the space of superseded old-format objects). Invoking it is
a deliberate act that accepts new hashes and a force-push.

acetone thus offers **both** strategies and the choice is the operator's,
defaulting to the non-destructive one. Nothing in the normal read/write path
ever rewrites history to cross a format boundary.

## Consequences

- **`acetone-5yr` is unblocked and scoped by this ADR.** It must turn
  `Manifest::decode`'s reject-on-mismatch into a dispatch over retained
  per-version decoders, keeping the v1 decoder as a distinct reader when v2 is
  introduced. Writes continue to emit `FORMAT_VERSION`.
- **The binary accretes one legacy reader per historical format.** This is a
  bounded, slow-growing cost (one format bump is a rare, deliberate event) and
  each retained decoder is exercised by a golden pinned at its version, so it
  cannot silently rot.
- **Old on-disk data is never eagerly upgraded.** A reader must always be
  prepared for any shipped version; there is no "all commits are current"
  invariant. This is the price of not rewriting history, and it is the point.
- **Introducing `format_version = 2` is Gate-D-class work.** The v2 bump
  (whatever change triggers it) lands with: the v1 decoder retained and pinned
  by golden, a v2 golden added, and a test that a v1 commit and a v2 commit
  coexist in one repository and both read correctly — satisfying Phase 8 exit
  criterion 3 (a live `format_version` bump with no history rewrite and no
  force-push).
- **Foreclosed:** the simplicity of a single-format-per-repo invariant. We
  accept mixed-version repositories as normal in exchange for never
  force-pushing shared history.
- **Revisit if:** the number of retained decoders becomes a real maintenance
  burden, or a future format change is *structurally* impossible to read
  forward (not merely an encoding change the dispatch can absorb). In that case
  rewrite-migrate becomes the recommended path **for that specific bump**,
  chosen deliberately and documented — it does not change this default for
  evolution in general.

This ADR is a policy decision only: it bumps no format, freezes nothing further,
and ships no code. It is a Phase 8 *forward gate* decided by ADR (per the
roadmap's forward-gate discipline) so implementation can proceed; it is flagged
for retrospective ratification in the Phase 8 report.
