# Phase 5 report — import/export and secondary indexes

*Prepared at the Phase 5 boundary for the sprint-demo review. Phase 5 is the
epic `acetone-6g5`. `main` is green at `3987874`.*

Phase 5 turns acetone from a graph you can branch and merge into one you can
**feed and query at speed**: bulk-import external sources as provenanced
commits, accelerate lookups with declared secondary indexes, export back out
for the relational world, and keep the object store compact and verifiably
intact. The through-line is the *scheduled-import* use case — a mutating source
synced into version-controlled history, its changes readable as diffs.

## What shipped

Five feature PRs (#65–#69), each behind a fresh-subagent adversarial review,
plus the exit simulation (#70):

- **Import plugin interface** (acetone-6g5.1, #65). A `SourceExtractor` trait
  and schema-driven transform in `acetone-graph` (no new core dependencies),
  with built-in **CSV** and **JSON/NDJSON** extractors in the thin CLI.
  `acetone import <csv|json|ndjson> <source> [--label L | --edge T --from … --to
  …] [--branch b]`. Each import is a commit carrying `Acetone-Source`,
  `Acetone-Extractor` and `Acetone-Source-Hash` (sha256) trailers; an unchanged
  source re-import is a **detected no-op**; `--branch` isolates the import and
  returns the caller to their branch. Authoritative-replace semantics (ADR-0021).
- **Declared property indexes** (acetone-6g5.3, #66 + #67). Split into storage
  and query halves.
  - *Storage* (6g5.3.1): the `idx/<name>` maps are maintained transactionally
    in the write path (delta per touched node; full-build for a new index),
    null- and NaN-blind; `Repository::reindex` rebuilds to identical roots
    (Invariant #5, property-tested); `declare-index` and `reindex` CLI; and an
    `fsck` index-consistency check.
  - *Query* (6g5.3.2): the binder's existing `IndexSeek` hint is wired through
    the executor — a per-query in-memory value map gives `O(matches)` seeks
    versus a label scan (ADR-0022). **~4.9× faster than scan** for
    `Host.os = 'debian'` on a 44k-node lab graph.
- **Export** (acetone-6g5.2, #68). `acetone export <csv|json|ndjson> [--label |
  --edge] [--out]` — one table per keyed label, one per relationship type; the
  seed of the relational projection (spec §9). JSON/NDJSON are the faithful
  round-trip formats (export → import into a fresh repo reproduces identical map
  roots, including the rebuilt `indexes` map); CSV is an explicitly lossy
  relational/spreadsheet export.
- **gc and fsck round-out** (acetone-6g5.4, #69). `acetone gc` consolidates the
  object store into a self-contained packfile, deltaing rewritten chunks against
  the predecessors the write path now records as base hints, and pruning
  superseded loose objects (ADR-0011). `fsck` gains a **history-independence
  spot-check** (rebuild the `nodes`/`edges_fwd` maps and compare roots — a
  non-canonical tree is an error), completing the spec §7 list alongside the
  index-consistency check.
- **Scheduled-import simulation** (acetone-6g5.5, #70). The flagship: three
  snapshots of a mutating source imported as commits, the unchanged one a
  no-op, `diff` between runs the change report, the index tracking the mutation
  — an integration test and a step-by-step demo script.

## Gate evidence — roadmap Phase 5 exit criteria

The roadmap's Phase 5 exit is *"a scheduled-import simulation — N successive
snapshots of a mutating source imported as commits, with no-op detection on
unchanged snapshots and `diff` between runs serving as the change report;
index-accelerated query demonstrably faster than scan on the lab graph; fsck
verifying index and reverse-edge consistency."*

- **Scheduled-import simulation** — `crates/acetone-cli/tests/scheduled_import.rs`
  and `scripts/phase-5-demo.sh`: snapshots v1 → v2 (mutated) → v3 (unchanged);
  v3 is a detected no-op; `diff(v1,v2)` reports `~ web1` and `+ new1` and omits
  the unchanged hosts; both real commits carry provenance trailers.
- **Index faster than scan** — the `lab` binary's index-vs-scan comparison:
  IndexSeek ~14 ms vs LabelScan+filter ~69 ms (~4.9×) on 44k nodes, verified by
  a parity test proving the seek returns exactly the scan's rows.
- **fsck index and reverse-edge consistency** — `check_index_consistency`
  (6g5.3.1) recomputes each declared index from `nodes`; edge-map symmetry
  predates this phase; both are seeded-corruption tested.

`cargo test --workspace`, `clippy -D warnings`, `fmt --check`, `cargo audit` and
`cargo deny` are green on every merged commit; the openCypher TCK conformance
job stayed green (no regression from the write-path and executor changes).

## Decisions taken (ADRs)

- **ADR-0021 — import plugin interface, provenance and no-op detection.** The
  interface lives in `acetone-graph` (no seventh crate); format extractors and
  their dependencies (`csv`, `serde_json`, `sha2`) are confined to the CLI;
  apply is authoritative-replace; the source hash is sha256 of the raw bytes;
  no-op detection is workspace-vs-HEAD.
- **ADR-0022 — IndexSeek execution over the materialised snapshot.** The
  workbench read path materialises a version once, so IndexSeek runs over an
  in-memory value map keyed by the runtime representation (not a lazy
  store-backed seek — that is the deferred scalability follow-up); numeric
  equality probes both Int and Float buckets; equality only (IndexRange
  deferred).

Both are mid-phase decisions taken by ADR so work could proceed, flagged here
for retrospective review.

## Review findings summary

Every PR passed a fresh-subagent adversarial review; the gate caught real,
shipping-blocking defects on **every** feature PR — the strongest evidence that
the review discipline is doing its job:

- **#65 import** — MAJOR: provenance-trailer validation happened *after*
  `save()`, so a source path with trailing whitespace left the workspace dirty
  and (under `--branch`) stranded the caller; fixed by validating trailers
  up front. Plus `--branch == current` rejection.
- **#66 index storage** — accepted with five minors, all fixed: precise
  encode-error handling (only NaN is policy-blind), fsck detecting a
  declared-but-missing index map, and added coverage.
- **#67 IndexSeek** — three rounds found three genuine *silent-wrong-results*
  subset bugs, each fixed and re-reviewed: Int/Float cross-type equality
  (`3 = 3.0`), the f64 ≥ 2^53 integral-float precision boundary, and stored
  `Bytes`/temporal keyed differently from the runtime representation the filter
  uses. A fresh reviewer confirmed no residual subset case.
- **#68 export** — two MAJORs fixed: path traversal via attacker-controlled
  label names in derived filenames (`safe_filename`), and a relationship type
  spanning more than one endpoint label pair silently producing wrong roots
  (now a loud rejection). CSV's lossiness was documented precisely.
- **#69 gc/fsck** — MAJOR data-safety: consolidation's reachability walk cannot
  see *other* linked worktrees' private refs, so a cross-worktree `gc` could
  prune their uncommitted or mid-merge state; fixed by refusing `gc` while
  linked worktrees exist, with the worktree-aware walk deferred.

## Milestone security review

A dedicated fresh-subagent security pass over the whole phase diff
(`8b6c7ff..HEAD`) — input handling, path/ref injection, panics on untrusted
data, destructive operations, resource governance, terminal injection,
dependency risk. **Verdict: the security gate is ready to close — no
blocker- or high-severity findings.** Everything the per-PR reviews hardened
was re-verified safe on the whole surface:

- **Verified safe:** provenance-trailer injection (source path validated
  *before* staging; import file contents never enter the commit message);
  export path traversal (`safe_filename` rejects absolute/`..`/separators/
  control chars); `--branch` ref injection (goes through `validated_ref_name`);
  the panic sweep (no reachable `unwrap`/`expect`/index/`as` panic on hostile
  schema, records, index values or import bytes — the one `index_entry_key`
  `.expect` is unreachable because `IndexDef` enforces non-empty at decode);
  deeply-nested JSON (serde_json's recursion limit + explicit nested-object
  rejection); the `gc` data-loss guard and consolidation reachability
  (fail-closed on linked worktrees, prune gated on the pack's actual object
  set with a full-reachable tripwire); and terminal/control-char injection
  (findings and names routed through `sanitise_line`/`format_label`).

Open items, none gating (tracked as beads, listed under Open risks):

- **F1 (medium)** — CSV export does not neutralise spreadsheet formula-trigger
  prefixes (`= + - @`), so an attacker-controlled property value like
  `=HYPERLINK(…)` could execute when the exported CSV is opened in Excel/Sheets.
  Off-terminal and operator-initiated; JSON/NDJSON are unaffected. Fix (prefix
  such cells with `'`) folded into `acetone-6g5.9`.
- **F2 (low)** — a whole-graph export where a label is named `rel-<X>` collides
  on disk with the edge table for rel-type `X` (truncating write). Folded into
  `acetone-6g5.9`.
- **F3 (low/accepted)** — import/export/fsck materialise the whole file/graph in
  memory with no cap. A bounded workbench assumption (single node, 64 MiB
  per-object store cap), not a remote DoS; the streaming extractor
  (`acetone-6g5.7`) and a bounded fsck check (`acetone-6g5.10`) are the eventual
  mitigations.

## Open risks and deferred work

Tracked as beads under the epic; none blocks the phase's shipped scope:

- **acetone-6g5.3.3** — IndexRange (range predicates), a lazy store-backed
  IndexSeek provider (the real scalability win, avoiding full materialisation),
  and KeySeek execution. The store-backed seek must mirror the numeric
  cross-type and representation fixes from #67.
- **acetone-6g5.7** — streaming/bounded-memory import for large sources (import
  currently reads the whole file and stages one transaction).
- **acetone-6g5.8** — import polish: distinct-count reporting, clearer
  unborn-branch `--branch` error, dead `Format::parse` arm.
- **acetone-6g5.9** — export hardening: move the relational projection to a
  lower crate (thin-client symmetry with import), self-describing edge export,
  composite-key edges, faithful CSV, Windows filename hardening.
- **acetone-6g5.10** — worktree-aware `gc` (enumerate every worktree's private
  refs, then drop the refuse-guard) and a streaming `fsck` canonicality check.
- **Known import/export limitation** — the CLI cannot declare property *types*
  yet, so CSV imports/exports store fields as strings; JSON/NDJSON preserve
  types. Import is upsert-only (no deletions), so a shrinking source snapshot
  leaves stale nodes — a full-sync mode is future work.

## The demo — the scheduled-import flagship

`scripts/phase-5-demo.sh` drives the real CLI step by step: declare a
`Host(name)` schema with an index on `os`; import snapshot 1 (three hosts);
import snapshot 2 (web1 re-imaged, a host added) — a second provenanced commit;
`diff` the two runs as the change report; re-import an unchanged snapshot for
the detected no-op; query `MATCH (h:Host {os:'windows'})` to see the index
track the mutation; and `fsck`/`gc` to show integrity and consolidation. The
live demo walks these one step per turn.
