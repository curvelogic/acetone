# ADR-0025: History-rewrite engine (`acetone migrate`)

*Status: accepted — ratified by Greg at the pre-0.1 boundary review (2026-07-11) · Date: 2026-07-09 · Bead: acetone-hsg*

## Context

Gate D (ADR-0024) froze `format_version = 1`. A freeze is only safe with an
escape hatch: when a future version bumps an encoding, a repository must be
brought forward. The roadmap calls this `acetone migrate` — "a working migrate
that rewrites history (new hashes accepted pre-1.0)".

The tension at Gate D: `migrate` brings an *old* format *up to current*, so
"current" must be the higher version — but we just froze current at v1, so
there is no genuine older or newer format to migrate between yet. Demonstrating
a real `format_version` bump would require inventing a second format (a
synthetic v0 to migrate from, or a v2 to migrate to), either of which is
artificial while v1 is the only real format.

**Greg's decision at the Gate D boundary (2026-07-09): "engine now, demo at
0.2".** Build the generic history-rewrite engine now and exercise it with a
*version-preserving* transform; defer the actual cross-version demonstration to
the first real 0.2 format change. No synthetic format.

## Decision

### A generic engine over a `FormatTransform`

`acetone_graph::migrate::rewrite_history(repo, transform)` rewrites all history
reachable from `refs/heads/*` and `refs/tags/*`:

1. Refuse a dirty or mid-merge workspace (the rewrite resets it).
2. Collect every reachable commit and topologically order it, parents first
   (Kahn's algorithm with a sorted ready-set, so the order is deterministic).
3. For each commit, decode its manifest, apply `transform` to get the new
   manifest, recompute anchors and summary, remap parents to their already
   rewritten commits, and write a new commit **preserving the message, author
   and committer — identity and timestamp — verbatim**.
4. CAS-swing every ref to its rewritten commit.
5. Reset the default workspace to the rewritten head.

The transform is the pluggable part: a future `format_version` bump implements
`FormatTransform` to re-encode each version, and the same engine rewrites the
graph.

### The shipped transform is a version-preserving re-chunk

The only transform today is `Rechunk`: rebuild every map under new chunk
parameters. This is genuinely useful (retuning chunk size) and, crucially, it
is the right *test vehicle* — it is **version-preserving** (chunk parameters
are manifest data; the key/value encodings and `format_version` are unchanged)
yet it rewrites every prolly root and therefore every commit hash (spec §3.2:
changing the chunking changes every hash). An identity transform would be a
no-op (history independence collapses it to the same hashes), so a
content-changing but version-preserving transform is exactly what exercises the
full engine — new hashes, parent remapping, ref swings, workspace reset —
without inventing a format. The CLI exposes it as `acetone migrate
--min-bytes --mask-bits --max-bytes`.

### Faithful metadata required a store extension

Rewriting history unfaithfully (restamping every commit to "now", dropping
authorship) would be a poor tool. `read_commit` dropped author/committer/time
and `create_commit` stamps "now", so the store gained: `Identity` (name, email,
git timestamp) exposed on `Commit`, and `GitStore::rewrite_commit`, which builds
the `.acetone/` tree from the transformed manifest but writes explicit
author/committer identities, timestamps and a verbatim message. This is the
minimal fidelity a credible history rewrite needs and is independently useful.

## Consequences

- `acetone-store`: `Commit` gains `author`/`committer: Identity`; new
  `RewriteCommit` spec and `GitStore::rewrite_commit`. `create_commit` and the
  new path share one tree-builder, so the `.acetone/` layout (ADR-0023) stays
  in one place.
- `acetone-graph`: new `migrate` module (`FormatTransform`, `Rechunk`,
  `rewrite_history`, `MigrateReport`).
- `acetone-cli`: `acetone migrate` runs the re-chunk over a repository.
- Determinism: same repository + transform ⇒ same new hashes, so re-running is
  idempotent (verified). `git fsck` stays clean; the superseded old commit
  graph becomes unreachable and is reclaimed by `gc`.
- **Deferred / limitations (tracked):**
  - The cross-version demonstration waits for the first real 0.2
    `format_version` bump, which implements a `FormatTransform` for the change.
  - Annotated-tag *objects* are not rewritten — a non-commit ref target is
    reported as `NotACommit` rather than silently skipped (a follow-up can
    deref-and-rewrite them, mirroring the `fsck` annotated-tag handling).
  - Ref swings are per-ref CAS, not a single atomic transaction; safe under the
    single-writer model, but a crash mid-swing leaves some refs advanced.
    Re-running migrate is idempotent, so re-running completes it.
