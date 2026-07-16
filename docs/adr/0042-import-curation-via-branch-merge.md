# ADR-0042: Import vs human curation — resolve via import-to-branch + merge

- Status: accepted
- Date: 2026-07-16
- Bead: acetone-6g5.11 (Greg's priority direction, Phase 5 boundary)
- Relates to: ADR-0021 (import), ADR-0035 (cell-wise merge), ADR-0041 (merge lifecycle), the Phase 6 dogfood.

## Context

Import is **authoritative-replace**: `put_node` overwrites the whole record for
a key (ADR-0021). In the registry dogfood a human curates nodes between
scheduled imports — adds an owner, a note, a risk annotation — properties the
source knows nothing about. A naive re-import replaces the record and blats the
human's curation. The source should own the fields *it* carries, not the
operator's.

Four options were weighed (see the bead): (1) a `--merge`/partial-update import
mode that upserts only source-declared properties; (2) a property-level
provenance / reserved curation namespace; (3) **import onto a side branch, then
`acetone merge`** — reusing the version-control merge machinery; (4) full-sync
with deletions. Greg's Phase-5-boundary direction was to prototype option 3
first and only build a bespoke import mode if it proved too heavy.

Since then, **cell-wise merge (ADR-0035)** landed. That materially changes the
calculus: option 3's old weakness was that a human edit and a source edit to the
*same node* conflicted whole-record, forcing manual resolution on every
re-import. With per-property merge, an annotation on a *different* property from
the ones the source carries auto-merges — no conflict at all.

## Decision

**Adopt option 3 as the answer for v0.2.** The workflow is:

- The scheduled importer runs `import --branch ingest` (the `--branch` isolation
  already shipped, ADR-0021): each run commits authoritative-replace records to
  a **one-directional source-mirror branch**, leaving the caller's branch
  untouched.
- A human curates on their own branch (e.g. `main`) and **merges `ingest` into
  it on their cadence** (`acetone merge ingest`). Cell-wise merge preserves a
  curation on a property the source does not carry (a one-sided change), takes
  the source's update to a source-owned property, and — when the human and the
  source changed the *same* property — surfaces a conflict-as-data the human
  resolves (`resolve`, or by writing the node), completing via the merge
  lifecycle (ADR-0041).

**No bespoke `--merge` import mode is built** (option 1); cell-wise merge plus
branch-and-merge covers the need without adding a second merge implementation to
the import path. A `--protect <props>` list remains a documented *fallback* if
the branch-and-merge cadence ever proves too heavy in practice. Options 2
(curation namespace / property provenance) and 4 (full-sync deletions) stay
deferred; option 4 in particular must be scoped to source-owned identity before
it can coexist with curation.

### Load-bearing workflow invariant

The mirror branch must stay **one-directional**: the importer commits only
source records to `ingest`, and the human only ever merges `ingest → main`,
**never `main → ingest`**. This keeps curation out of every merge base, so a
curated property is always a one-sided add on the human's side and is preserved
indefinitely across arbitrarily many re-import cycles.

The footgun to avoid: merging `main → ingest` (or otherwise letting a curated
property reach `ingest`'s history) leaks the annotation into the merge base.
Then a later authoritative-replace re-import — which drops the property, because
the source does not carry it — reads as "theirs deleted it, ours unchanged from
base" and the merge takes the deletion, silently losing the annotation. The
one-directional discipline is what prevents this; it is a property of the
workflow, not enforced by the store.

## Consequences

- The registry dogfood's core tension is resolved with shipped machinery and no
  new format or import surface: curation is preserved as a non-conflicting merge
  or surfaced as conflict-as-data, never silently lost — provided the mirror
  stays one-directional.
- Evidence: `crates/acetone-graph/tests/import_curation_merge.rs` drives the flow
  end-to-end — a human annotation preserved across a re-import, stability across
  six re-import cycles, and a same-property clash surfacing as a conflict that
  resolves and completes.
- Deferred: option 4 (full-sync deletions, scoped to source-owned identity) and
  a first-class `--protect`/curation-namespace mode remain open if operational
  experience calls for them. This ADR does not close the dogfood-continuity exit
  criterion (acetone-zlu #5), which needs Greg's operational evidence.
