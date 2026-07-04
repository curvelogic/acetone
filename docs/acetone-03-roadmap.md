# Acetone — Roadmap and Implementation Plan

*v0.1, July 2026. Phases are sequential with hard exit criteria; sizes are relative (S/M/L/XL) rather than calendar estimates, on the assumption of agentic implementation via Claude Code with human review at the gates.*

## Guiding principles

Correctness properties are enforced by tests before features accrete: history independence, encoding determinism, merge determinism and TCK conformance are all mechanically checkable, and each phase's exit criteria lean on them. The riskiest assumptions are retired first (Phase 0 exists solely to de-risk the git-as-chunk-store bet before anything is built on it). Every phase ends with something usable from the CLI, however small, because a workbench product should feel like a product early.

## Phase 0 — Feasibility spike (size S)

Build a throwaway prolly map over the git object database: content-defined chunking (~4 KiB target), put/get/scan, root-manifest-in-a-git-commit, using gitoxide with git2 as fallback. Evaluate the existing `prollytree` crate hands-on as part of this — adopt, fork, or treat as reference. Benchmark against the representative asset-registry envelope: 100k, 1M and 5M keys; bulk load, point read, range scan, single-key update (measuring write amplification), diff between adjacent versions, repo size after 100 simulated import commits, and repack behaviour (loose objects vs pack-first writing).

Exit criteria: history independence demonstrated under randomised operation orders (property test); update latency and repo growth acceptable at 1M keys; a written decision on gitoxide vs git2 for the write path and on adopt-vs-build for the tree; go/no-go on Decision 1 Option A. If no-go, pivot to the hybrid Option C design before proceeding — nothing above this layer changes either way, which is the point of doing this first.

## Phase 1 — Storage and model core (size L)

Implement `acetone-store`, `acetone-prolly` and `acetone-model` per spec §3: the ChunkStore trait and git backend; production prolly trees with diff and three-way merge; order-preserving key encoding and canonical CBOR values with exhaustive round-trip and ordering property tests; schema map, node/edge/index map layouts; manifest and commit plumbing; workspace refs; the single-writer lock. CLI: `init`, `status`, `commit`, `log`, `branch`, `checkout`, plus low-level `put-node`/`get-node` plumbing commands for testing. Also `fsck` in skeletal form — building the verifier alongside the format pays for itself immediately.

Exit criteria: a scripted end-to-end — init, insert nodes/edges via plumbing, commit, branch, mutate, diff roots — with `git log` and `git push` to a real GitHub private repo working untouched; property-test suite green (history independence, encoding order, merge determinism at the map level); fsck clean.

## Phase 2 — Cypher read path (size L)

`acetone-cypher` front end and read-only execution per spec §5.3: parser adoption (decypher evaluation vs vendored openCypher grammar — decide in week one of the phase), binder against schema, logical plan, iterator-model operators for scan/seek/expand/filter/project/aggregate/sort/limit, `OPTIONAL MATCH`, `UNWIND`, parameters, and openCypher null semantics. Stand up the openCypher TCK harness immediately and drive development from it. CLI: `query` and the `shell` REPL with table/JSON output.

Exit criteria: TCK read-scenario pass rate ≥ a chosen bar (suggest 60% of read features as the honest MVP line, published, with a tracked climb thereafter); a lab asset graph (say, 50k nodes / 200k edges of hosts, software, suppliers, certificates) queryable with realistic registry queries at interactive latency; `AT <ref>` time travel over the Phase 1 commit graph.

## Phase 3 — Cypher write path and the commit discipline (size M)

`CREATE`, `MERGE` (upsert-on-key), `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`; transactional write batching into workspace roots; constraint enforcement (key presence, uniqueness, existence) at write and commit; key-immutability rule and the `rekey` utility. This is the phase where the workbench loop closes: checkout → edit via Cypher → status/diff → commit.

Exit criteria: write-feature TCK scenarios passing for the supported subset; idempotence demonstrated — re-running a `MERGE`-based load of identical data produces an unchanged root and `commit` reports nothing to commit; interactive editing session captured end-to-end in docs.

## Phase 4 — Diff, merge and conflicts (size L)

Graph-level `diff` (classified node/edge change streams and the `_Added`/`_Removed`/`_Modified` virtual graph), merge-base computation over the git commit graph, map-wise three-way merge, post-merge graph validation (dangling edges, constraint re-check), the `conflicts` map, `acetone.conflicts()` and `_Conflict` querying, `resolve`, and merge commits. Property-based testing regime: generate random base graphs and divergent edit scripts; assert merge determinism, symmetry properties where applicable, and that clean merges never produce dangling edges.

Exit criteria: the flagship demo works — two branches import overlapping asset data, merge produces both clean results and representative conflicts, conflicts are inspected and resolved in Cypher, history shows the whole story; blame implemented for nodes.

## Phase 5 — Import/export and secondary indexes (size M)

The `import` plugin interface (CSV and JSON/NDJSON built in; a plugin trait for source-system extractors), deterministic transform convention, provenance trailers, `--branch` import isolation; `export` to CSV/JSON (the seed of the relational projection); declared property indexes with `IndexSeek` planner integration and `reindex`. Round out `gc` and `fsck`.

Exit criteria: a scheduled-import simulation — N successive snapshots of a mutating source imported as commits, with no-op detection on unchanged snapshots and `diff` between runs serving as the change report; index-accelerated query demonstrably faster than scan on the lab graph; fsck verifying index and reverse-edge consistency.

## Phase 6 — Hardening towards 0.1 (size M)

Format freeze review of spec §3.4 encodings and manifest (with `format_version` machinery and a working `migrate`), TCK pass-rate push, error-message quality, docs (user guide, format spec, conformance statement), packaging (single static binary; `acetone-core` published as a library crate), and a dogfood deployment: the real asset-registry use case run in anger on a private GitHub remote.

Exit criteria: 0.1 tagged; a fortnight of dogfooding without data-integrity incidents; the three documents in this pack revised to match reality.

## Beyond 0.1 (unscheduled, in rough order of pull)

Views and materialised views over the provider-pluggable scan interface; the virtual-element provider API generalised from the conflict/diff machinery; RDF projection (oxigraph-backed SPARQL over exports first, live adapter later); relational/Parquet projection; costed query planning and var-length path performance; a native chunk store behind the existing trait if scale demands it; `CALL {}` subqueries and GQL-alignment syntax work; optional read-only server mode for dashboards (a deliberate late arrival — the workbench identity comes first).

## Decision gates and open questions

Gate A (end Phase 0): git ODB as chunk store — confirmed or pivoted. Gate B (start Phase 2): parser adoption vs vendored grammar. Gate C (end Phase 2): TCK bar for MVP honesty. Gate D (Phase 6): format freeze.

Open questions to settle during Phases 1–2: SHA-1 vs SHA-256 default for new repos (leaning SHA-256 for new-format cleanliness vs SHA-1 for hosting ubiquity — verify current GitHub SHA-256 support before deciding); exact `AT` syntax versus GQL's graph-reference direction; whether edge records are duplicated into `edges_rev` (space for locality) or not (the spec currently says not); discriminator design for parallel edges (declared property vs positional); and whether `schema` changes should require their own commits (leaning yes — schema migrations as first-class history is very much in the spirit of the thing).

## Suggested repo scaffolding

Cargo workspace `acetone/` with the six crates from spec §8, `tck/` vendoring the openCypher TCK runner, `benches/` holding the Phase 0 suite kept alive as regression benchmarks, and `docs/` holding these three documents as the living design record. CI: test + property-test + TCK-conformance report as a published artefact per commit — a database that version-controls data should be embarrassed by anything less for its own code.
