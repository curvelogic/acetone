# ADR-0062: Streaming import — pull-based extraction, batched staging

Status: accepted (Phase 9, acetone-6g5.7); recorded for boundary review

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
   deliberate pre-1.0 breaking change to the (frozen-listed, not
   semver-promised) `acetone-cypher`/`acetone-core` import surface;
   snapshots re-blessed per ADR-0046.
2. **Batched staging.** The importer pulls, canonicalises,
   constraint-checks and stages records in batches
   (`ImportOptions::batch_size`, default 8192), saving each batch's
   transaction and committing **once** at the end. No-op detection
   (`is_dirty`) is unchanged. The final graph is batch-size independent —
   identical committed manifests at any batch size (property-tested;
   Load-Bearing Invariants #1 and #5).
3. **Extractor contract.** A source must yield a node before any edge
   that references it. Referential integrity is enforced at each batch's
   transaction boundary (ADR-0028), so a forward reference across batches
   fails the import. (The built-in CLI extractors are homogeneous per run
   — all nodes or all edges — and unaffected.)
4. **Streaming constraints.** Existence (`REQUIRE`) is checked per
   record. UNIQUE claims are tracked per `(label, property, value)` with
   replace-semantics unclaiming, seeded by one streaming pass over the
   workspace at import start — only when a label declares UNIQUE. Memory
   is O(claimed unique values), inherent without a persistent index;
   index-backed UNIQUE (acetone-ryg) is the eventual fix. Last record
   for a key wins *within a batch* before checks, as before; a violating
   record that only a **later** batch would supersede, or a transient
   UNIQUE collision a later batch would resolve, errors under streaming —
   the price of not buffering the source.
5. **Failure cleanup.** A mid-stream failure after any batch has saved
   resets the workspace to its committed state
   (`Repository::reset_workspace_to_head`, the `abort_merge` primitive
   exposed; also the substrate for a future workspace-discard command,
   acetone-omk). The old "extract before touching the workspace"
   guarantee is thereby preserved in effect: an import either commits
   whole or leaves the workspace as it found it.
6. **CLI.** CSV and NDJSON parse incrementally from a `BufReader` (the
   `csv` crate is already streaming; NDJSON is line-wise). The source is
   read twice — a 64 KiB-chunked SHA-256 pass for the provenance hash,
   then the parse — keeping trailer validation ahead of any staging at
   O(1) hashing memory. A JSON *array* source remains whole-parsed (a
   single value): documented residual — use CSV or NDJSON for sources
   larger than memory.

## Consequences

- Bounded resident memory: O(batch + tree caches + UNIQUE claims),
  independent of source size, for CSV/NDJSON sources.
- Two semantic sharpenings, both error-side (never silent): cross-batch
  forward references and cross-batch transient constraint states now
  fail; in-batch behaviour is unchanged.
- `export`/`fsck` whole-graph materialisation (the other half of F3) is
  unchanged and tracked separately (acetone-6g5.9, acetone-6g5.10).
