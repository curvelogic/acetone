# ADR-0062: Streaming import — pull-based extraction, batched staging

Status: accepted (Phase 9, acetone-6g5.7); **ratified by Greg at the Phase 9 boundary, 2026-07-31**

## Context

Import held three whole-source copies in memory: the raw file bytes, the
extracted `Vec<ImportRecord>`, and a single staging transaction — plus an
in-memory `NodeSet` of the *entire* workspace for the constraint check
(Phase 5 security review F3, accepted then as a bounded-workbench
limitation). Phase 9's exit criteria require a source larger than memory
to import in bounded resident memory.

## Decision

1. **Pull-based extraction.** `SourceExtractor::extract() -> Vec<_>`
   becomes `next_record() -> Result<Option<ImportRecord>>`. A
   `VecExtractor` covers callers whose source fits in memory. This is a
   deliberate pre-1.0 breaking change to the import surface, which sits
   outside the ADR-0046 frozen snapshot lists (the freeze check passed
   untouched; no re-bless was needed).
2. **Batched staging.** The importer pulls, canonicalises,
   constraint-checks and stages records in batches
   (`ImportOptions::batch_size`, default 8192; reachable from the CLI as
   `--batch-size`), saving each batch's transaction and committing
   **once** at the end. No-op detection (`is_dirty`) is unchanged. The
   final graph is batch-size independent — identical committed manifests
   pinned by a fixed-input test at batch sizes 1/3/whole (Load-Bearing
   Invariants #1 and #5), corroborated by the pre-existing
   incremental-index-matches-reindex property test.
3. **Extractor contract.** A source must yield a node before any edge
   that references it. Referential integrity is enforced at each batch's
   transaction boundary (ADR-0028), so a forward reference across batches
   fails the import. (The built-in CLI extractors are homogeneous per run
   — all nodes or all edges — and unaffected.)
4. **Streaming constraints.** Existence (`REQUIRE`) is checked per
   record. UNIQUE claims are tracked compactly — interned
   `(label, property)` pairs, each distinct claimed value's encoding
   stored once, owners flagged *imported* — seeded by one **lazy** pass
   over the workspace node map at import start (keys decode first;
   records of non-unique labels are never decoded), only when a label
   declares UNIQUE. A violation is reported only when an imported record
   is among the colliding owners: a pre-existing breach the import does
   not touch, or actively shrinks, stays fsck's business (the old focus
   semantics). Memory is **O(nodes of unique-constrained labels)** —
   claims for the workspace's and the source's unique values both —
   inherent without a persistent index; index-backed UNIQUE
   (acetone-ryg) is the eventual fix. Last record for a key wins
   *within a batch* before checks, as before; a violating record that
   only a **later** batch would supersede, or a transient UNIQUE
   collision a later batch would resolve, errors under streaming — the
   price of not buffering the source.
5. **Failure cleanup.** A mid-stream failure after any batch has saved
   resets the workspace to its committed state
   (`Repository::reset_workspace_to_head`, an `abort_merge`-style reset
   that leaves `MERGE_HEAD` untouched; also the substrate for a future
   workspace-discard command, acetone-omk). The old "extract before
   touching the workspace" guarantee is thereby preserved in effect: an
   import either commits whole or leaves the workspace as it found it.
6. **CLI.** CSV and NDJSON parse incrementally from a `BufReader` (the
   `csv` crate is already streaming; NDJSON is line-wise). The source is
   read twice **through one file handle** — a 64 KiB-chunked SHA-256
   pass, a rewind, then the parse — keeping trailer validation ahead of
   any staging at O(1) hashing memory while making it impossible for the
   hash and the parse to describe different files (a path swap between
   passes cannot bite; in-place mutation of an open file during import
   remains inherently racy, exactly as with the old single read). A JSON
   *array* source remains whole-parsed (a single value): documented
   residual — use CSV or NDJSON for sources larger than memory. A failed
   `--branch` import deletes a side branch it created (streaming means
   branch creation now precedes parsing).

## Consequences

- Resident memory for CSV/NDJSON sources is O(batch + tree caches),
  independent of source size, **when no imported label declares
  UNIQUE** (measured: 154 MB/1M-row source -> 125.7 MB maxrss;
  621 MB/4M rows -> 193.5 MB). With UNIQUE declared, the claim tracker
  adds O(nodes of unique-constrained labels) — the Phase 9
  larger-than-memory criterion is met for UNIQUE-free schemas and
  *scoped* accordingly for UNIQUE until acetone-ryg lands.
- Two semantic sharpenings, both error-side (never silent): cross-batch
  forward references and cross-batch transient constraint states now
  fail; in-batch behaviour is unchanged.
- `export`/`fsck` whole-graph materialisation (the other half of F3) is
  unchanged and tracked separately (acetone-6g5.9, acetone-6g5.10).
