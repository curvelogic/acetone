# Acetone — Specification Draft

*v0.1 draft, July 2026. Normative language (MUST/SHOULD/MAY) used loosely at this stage; this is a spec skeleton to be hardened during Phase 1.*

## 1. Scope and non-goals

Acetone is an embedded, single-node, version-controlled labelled property graph database with an openCypher query interface and a git-compatible storage backend. It is a workbench: invoked as a CLI and library within a repository directory, not operated as a network service. Out of scope for this specification: multi-writer concurrency, network query protocols (Bolt etc.), horizontal scale, columnar analytics performance, and the view/virtual/RDF/relational projection layers (reserved in §9 so their hooks are stable).

## 2. Data model

A **repository** contains one **graph** (multi-graph support reserved). A graph comprises **nodes** and directed **relationships**, each bearing **labels**/**type** and **properties**.

A node has exactly one **primary label** and zero or more secondary labels. Properties are a map from string names to values drawn from: null, boolean, integer (i64), float (f64), string (UTF-8), bytes, date, time, datetime (with offset), duration, and homogeneous lists of the foregoing. Nested maps are excluded from v0.1 (revisit for GQL alignment).

**Schema is mandatory for identity, optional for shape.** Each primary label MUST declare a **key**: an ordered, non-empty tuple of property names whose values MUST be present, non-null and scalar on every node bearing that label. The pair `(primary label, key values)` is the **node key** and is the node's identity for storage, diff and merge. A label MAY additionally declare property types and constraints (v0.1 supports `UNIQUE` on non-key properties and existence constraints; both enforced at commit time and merge time).

A relationship is identified by `(source node key, relationship type, target node key, discriminator)`. The discriminator defaults to the empty value; parallel relationships of the same type between the same endpoints MUST declare a discriminator property in schema. Relationships have properties as nodes do, and no labels beyond their type.

Surrogate identity: a label MAY be declared `KEY SURROGATE`, in which case acetone mints a ULID key property `_id` at creation. Documented consequence: concurrent creation across branches merges as distinct nodes.

## 3. Storage layer

### 3.1 Chunk store

All persistent structures are serialised into **chunks** stored in a content-addressed chunk store behind the `ChunkStore` trait: `put(&[u8]) -> Hash`, `get(&Hash) -> Option<Bytes>`, plus ref and commit operations. The v0.1 reference implementation is the **git object database** of the enclosing repository (via gitoxide, with git2 fallback where write-path features are missing): a chunk is a git blob and its address is the git object ID. The repository's git object format (SHA-1 default, SHA-256 supported) determines hash width; acetone treats hashes as opaque. Per-commit writes land as loose objects (git-clean and cheap); the retention win comes from `acetone gc`, which is acetone's **own** periodic consolidation — it rewrites the reachable object set into a self-contained packfile of REF_DELTAs against the predecessors acetone chooses at write time (ADR-0011). It does not delegate to git maintenance: git's own heuristics never pair a content-addressed chunk with its predecessor, so **running stock `git gc`/`git repack` on an acetone repository is safe-but-lossy** — it corrupts nothing but discards the hand-chosen deltas, and re-running `acetone gc` restores them. Consolidation is representation-only: it preserves every object's bytes and address exactly.

### 3.2 Prolly trees

Every persistent map is a prolly tree over the chunk store. Keys and values are byte strings under the encodings of §3.4. Chunk boundaries are determined by a rolling/content-defined split function targeting a mean chunk size of 4 KiB (configurable per repo, fixed at init, recorded in the manifest header — changing it changes every hash). The split function, serialisation and tree construction MUST be deterministic and history-independent: identical map contents MUST yield identical root hashes regardless of operation order. This property is normative; a property-based test suite enforces it.

Required tree operations: point get, range scan (forward and reverse), batched insert/delete producing a new root, structural diff between two roots yielding an ordered stream of (key, before, after), and three-way merge given a base root (returning merged root plus a stream of key-level conflicts).

### 3.3 Maps and the manifest

A graph version is a **manifest**: a small canonical record listing the root hashes of its constituent maps plus format metadata. v0.1 maps:

`schema` — label definitions, key declarations, constraints, index declarations. `nodes` — node key → node record (secondary labels, properties). `edges_fwd` — (src key, type, dst key, disc) → edge record (properties). `edges_rev` — (dst key, type, src key, disc) → empty. `idx/<name>` — declared property indexes, (label, property, encoded value, node key) → empty. `conflicts` — present only in a merge-in-progress workspace; see §6.

`edges_rev` MUST be maintained transactionally with `edges_fwd`; indexes MUST be consistent with `nodes` in every committed manifest (they are derived data and MAY be rebuilt with `acetone reindex`, which MUST reproduce identical roots).

### 3.4 Encodings

Keys use an order-preserving tuple encoding (memcomparable): type-tagged, big-endian integers with sign flip, IEEE-754 total-order transform for floats, length-framed UTF-8, so that byte order equals logical order and range scans equal label/prefix scans. Values use canonical deterministic CBOR (definite lengths, sorted map keys). Any change to either encoding is a format version bump in the manifest header.

### 3.5 Commits and refs

An acetone commit is a **git commit object**. Its tree carries acetone's machine-readable state — the manifest and the chunk-anchor tree — under a reserved `.acetone/` directory (`.acetone/manifest`, `.acetone/chunks/`), plus a small human-readable `README.md` summary at the **tree root** so hosting UIs show something meaningful when browsing a repository (ADR-0023); the workspace tree under `refs/acetone/workspaces/*` uses the same `.acetone/` layout without the root README. Namespacing the machine entries keeps the tree root free for co-tenant files. Parents, author, committer, message and signing are git-native. Structured metadata (import provenance, tool versions) is carried in commit message trailers (`Acetone-Source:`, `Acetone-Extractor:`, `Acetone-Source-Hash:`). Branches and tags are git refs under the usual namespaces. The working set is a manifest referenced from `refs/acetone/workspaces/<name>` (default workspace per checkout), giving Dolt-style WORKING state that survives process exit. Push/pull/clone are git push/pull/clone; a remote need not know acetone exists.

## 4. Transactions and concurrency

Single writer per repository, enforced by a lock file; unlimited concurrent readers, each pinned to an immutable root (MVCC by construction — snapshot isolation is free). A write transaction accumulates map mutations in memory (spilling batched tree writes as needed), applies constraint checks, and atomically advances the workspace ref on success. `commit` is a separate act that turns the workspace manifest into a git commit and resets the stage. Crash safety follows from content-addressing: unreferenced chunks are garbage, refs advance atomically.

## 5. Query language

### 5.1 Conformance

The query language is **openCypher**, measured against the openCypher TCK. Each release MUST publish its TCK pass rate. v0.1 target subset (Level R — read): `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN` (with `DISTINCT`, aliases), `WITH`, `ORDER BY`, `SKIP`, `LIMIT`, `UNWIND`, parameters, literals, the core expression language including `CASE`, list/string functions, aggregation (`count`, `sum`, `avg`, `min`, `max`, `collect`), pattern predicates, and variable-length relationship patterns with bounds. Level W — write: `CREATE`, `MERGE` (with `ON CREATE SET`/`ON MATCH SET`), `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`. Explicitly deferred: `FOREACH`, `CALL {}` subqueries, shortest-path functions, full temporal arithmetic, map projections. Where openCypher and GQL:2024 diverge, new syntax choices SHOULD follow GQL.

Write semantics interact with identity: `CREATE` of a node whose key already exists is an error; `MERGE` on a full key pattern is the canonical upsert; `SET` MUST NOT modify key properties (key changes are modelled as delete-plus-create, i.e. a new identity — a `rekey` utility MAY assist and record both sides in one commit).

### 5.2 Versioning surface

Session state includes the checked-out ref; queries address that state by default. Time travel: `AT <refspec>` may suffix a `MATCH` clause group (`MATCH (n:Host) AT 'main~5' RETURN n`), resolving any git refspec. History procedures: `CALL acetone.log([ref])`, `CALL acetone.diff(from, to)` (yielding change rows and, in graph form, `_Added`/`_Removed`/`_Modified` virtual elements), `CALL acetone.blame(label, key)`, `CALL acetone.conflicts()`. Procedures are read-only; repository mutations (branch, merge, commit) are CLI/library operations, not query-language operations, in v0.1.

### 5.3 Execution model

Parser (decypher or equivalent, producing a spanned AST) → binder (resolves labels, keys, indexes against `schema`) → logical plan → physical plan of iterator-model operators: `LabelScan`, `KeySeek`, `IndexSeek/Range`, `ExpandOut`/`ExpandIn` (edge-map range scans), `Filter`, `Project`, `Aggregate`, `Sort`, `Limit`, `VarExpand`, `Apply` (for `OPTIONAL MATCH`/pattern predicates), `Create/Merge/Set/Delete` sinks. Planning in v0.1 is heuristic (use an index when one covers a predicate; order expansions by declared selectivity hints); costed planning is deferred. Null semantics follow openCypher exactly and are TCK-verified.

## 6. Diff and merge

`diff(a, b)` streams map-level structural diffs and classifies them into node/edge added/removed/modified records. `merge(theirs)` computes the merge base via the git commit graph, three-way-merges each map, then runs graph validation: dangling-edge detection (edge present, endpoint absent) and constraint re-validation over the changed key set. Clean merges produce a merge commit directly. Otherwise the workspace enters merging state: the `conflicts` map is populated with structured records (key, base/ours/theirs values, or violation class for graph-level conflicts), queryable via `acetone.conflicts()` and the `_Conflict` virtual subgraph; the user resolves by ordinary writes plus `acetone resolve <key>|--all-ours|--all-theirs`; `acetone commit` completes the merge. Merge MUST be deterministic given (base, ours, theirs).

## 7. CLI surface

`acetone init [--object-format sha256] | status | log | diff [<a> [<b>]] | branch | checkout <ref> | commit -m | merge <ref> | resolve | push | pull | clone <url> | query '<cypher>' [--at <ref>] [--format table|json|csv] | shell | import <plugin> <source> [--branch] | export <format> | reindex | gc | fsck`. `shell` is a readline Cypher REPL with `:checkout`, `:log`, `:diff` conveniences. `fsck` verifies chunk reachability, manifest integrity, edge-map symmetry, index consistency and history independence spot-checks. All commands are also exposed as a Rust library API (`acetone-core`), which is the real product surface; the CLI is a thin client, keeping the door open to Telegram/agent frontends and editor integrations.

## 8. Crate layout

`acetone-store` (ChunkStore trait; git backend; refs/commits) · `acetone-prolly` (trees, diff, merge; property-tested for history independence) · `acetone-model` (keys, records, encodings, schema, manifest) · `acetone-graph` (graph mutations, constraints, validation, merge orchestration) · `acetone-cypher` (front end, planner, executor; TCK harness) · `acetone-cli`. Dependency direction is strictly downward in that list.

## 9. Reserved for future layers

Views: named Cypher definitions stored in `schema`, addressed like labels. Materialised views: derived map sets whose manifests record (definition hash, source root); refresh is recomputation, staleness is hash inequality. Virtual nodes/relationships: the `_Conflict`/diff machinery of §5.2/§6 is the prototype of a general virtual-element provider interface in the executor — implementers should keep `LabelScan` provider-pluggable from the start. RDF projection: IRIs minted from node keys under a per-repo base IRI recorded in `schema`. Relational projection: one table per keyed label; also the CSV/Parquet export contract. Bitemporality: transaction time is the commit graph; valid time, if ever needed, becomes ordinary properties plus query sugar — never a storage change.

## 10. Format stability

Until 1.0, the manifest carries `format_version`; any change to key encoding, value encoding, chunking parameters or manifest schema increments it, and `acetone migrate` rewrites history (producing new hashes — this is understood and acceptable pre-1.0, and a strong reason to harden §3.4 early).
