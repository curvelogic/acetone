# Acetone — Design Space, Prior Art and Key Decisions

*Working document, v0.1 — July 2026*

## Vision

Acetone is a version-controlled property graph database in the style of Dolt: a workbench product, not a SaaS. It stores a labelled property graph in content-addressed prolly trees laid out in a git-compatible commit graph, and exposes openCypher as the local query language. The primary use case is an asset registry and inventory capability in which change arrives either through manual pull–edit–push workflows or through imports that are logged as commits, and in which full, queryable history is a first-class requirement.

The core deliverable — the first building block — is tightly scoped: a Dolt-style, Cypher-supporting graph database. Virtual nodes and relationships, views and materialised views, and relational and RDF projections of content are explicitly designed *for* but deferred; the core must not preclude them.

Implementation language is Rust throughout.

## The design space

There is no existing system that occupies acetone's exact position. The nearest neighbours each hold two of the three defining properties (versioned storage, property-graph/Cypher, embedded workbench) but never all three.

**Dolt** is the reference architecture for the versioning half. Its storage engine is a git-style commit graph of prolly trees: table schema and data live in prolly trees, and the roots of those trees, plus metadata, are hashed together into commits forming a Merkle DAG. Prolly trees give it B-tree-like read/write performance, diffs in time proportional to the size of the difference rather than the data, and structural sharing so that unchanged portions are stored once across all versions. Two properties matter enormously and should be treated as load-bearing for acetone: *history independence* (the same key–value content always produces the same tree, regardless of the order of operations that produced it) and cell-wise three-way merge anchored on primary keys. Crucially, Dolt does **not** use git's object store — it reimplements git semantics over a custom content-addressed block store (descended from Noms) with chunks averaging around 4 KB. That was a deliberate engineering choice, and acetone must decide consciously whether to follow it or diverge (see Decision 1).

**TerminusDB** is the closest thing to a versioned graph database in production. It is an RDF-based document/graph store with git-like branch, merge, diff, clone and time-travel, implemented as immutable append-only delta layers over succinct data structures, with the storage core written in Rust. It validates the demand for "git for graphs" and its delta-layer model is a genuine alternative to prolly trees — but it is RDF/document-shaped rather than a labelled property graph, it queries via WOQL (Datalog) and GraphQL rather than Cypher, and it is architecturally a server. Its delta-layer approach is worth studying for the future RDF projection, and as a cautionary contrast: delta chains require rollups/squashing to keep read performance, whereas prolly trees materialise full state per version and pay in write amplification instead. For a workbench with modest write rates and a premium on fast diff, the prolly-tree trade is the right one.

**Kuzu** was the state of the art in embedded Cypher databases — columnar storage, CSR adjacency indexes, vectorised and factorised execution, full Cypher, MIT-licensed, in-process. Its trajectory is a significant recent fact in this design space: the upstream repository was archived in October 2025 following an Apple acqui-hire, and the community has continued the architecture through forks, principally LadybugDB. Kuzu has no version control whatsoever, and its columnar layout is essentially incompatible with content-addressed structural sharing (Dolt themselves have written about the difficulty of marrying prolly trees with columnar storage). Kuzu's value to acetone is as a *query-engine design reference* — schema-typed node and relationship tables, vectorised operators, join strategies — and as a warning about ecosystem risk in adopting large external engines.

**The Rust openCypher ecosystem** turns out to be healthier than expected, which changes the build-vs-buy calculus for the query layer. Three findings stand out. First, **ocg** is a pure-Rust openCypher engine claiming 100% openCypher TCK compliance (3,874 of 3,897 scenarios passing, the remainder skipped) over in-memory property graphs with pluggable backends, built on a pest parser. Second, **decypher** is a standalone, error-resilient, rowan-based Cypher parser producing a typed AST with source spans — a rust-analyzer-style lossless CST, explicitly designed for reuse in query rewriting and tooling. Third, the **openCypher TCK** itself (Cucumber feature files maintained by the openCypher project) provides a ready-made conformance suite regardless of which engine path is taken. The strategic backdrop is that openCypher is now formally evolving towards **ISO/IEC 39075:2024 GQL**, published April 2024 — the first new ISO database language since SQL — so acetone should implement openCypher today while keeping the grammar and AST layer positioned for GQL convergence.

**The Rust prolly-tree ecosystem** contains one directly relevant project: the **prollytree** crate (v0.3.x), which implements probabilistic B-trees with in-memory, RocksDB and — notably — *git-backed* storage, plus diff, sync and three-way merge, and even a GlueSQL integration. Its git `versioned_store` performs blocking I/O through the git object database. It is young and its production-readiness is unproven, but it is proof of feasibility for the exact storage combination acetone wants, and a candidate for either adoption, fork, or (most likely) a from-scratch implementation informed by its design and by Dolt's documentation of chunking behaviour.

**gitoxide (gix)** is the natural git substrate: a pure-Rust git implementation whose library crates are used in production by cargo, Helix and GitButler. Object database read/write, refs, commit-graph traversal, diff, clone and fetch are solid; push and full merge workflows are still maturing (with `git2`/libgit2 bindings as the fallback where gaps bite). SHA-256 object-format support has been landing recently, which matters for Decision 1.

Peripheral prior art worth keeping on the radar: **Cozo** (embedded Rust Datalog with time travel) for its treatment of temporality; **XTDB/Datomic** for bitemporal semantics if the asset registry later needs valid-time as well as transaction-time; **oxigraph** (Rust RDF/SPARQL) as the obvious partner for the future RDF projection; **Fluree** and **Irmin** as other points in the versioned-data space; and **git-bug / gitqlite**-style projects as demonstrations of structured data living happily inside real git repositories at workbench scale.

### Positioning summary

| System | Versioned storage | Property graph / Cypher | Embedded workbench | Rust |
|---|---|---|---|---|
| Dolt | prolly trees + commit graph | no (SQL) | yes (CLI + server) | no (Go) |
| TerminusDB | delta layers | no (RDF, WOQL/GraphQL) | no (server) | core only |
| Kuzu / LadybugDB | no | yes, full Cypher | yes | no (C++) |
| ocg | no | yes, TCK-complete, in-memory | library | yes |
| prollytree crate | git-backed prolly KV | no | library | yes |
| **acetone** | **prolly trees on git** | **openCypher** | **yes** | **yes** |

## The six decisions that shape the design

### Decision 1 — What "git as the backend store" actually means

Three interpretations exist, and the choice ripples through everything.

**Option A: a real git repository.** Prolly-tree chunks are stored as git blobs; the chunk's content address *is* its git object ID; the per-commit root manifest is a small git tree/blob; acetone commits are genuine git commit objects whose tree points at the manifest; branches are git refs. The overwhelming attraction is that the entire git ecosystem comes for free: `git clone`, push/pull to GitHub or any remote (including private repos), hosting, signed commits, CI triggers, backup, and mental-model continuity for anyone who knows git. The risks are performance-shaped: git's loose-object store degrades with very large numbers of small objects and needs periodic repacking; packfile delta compression is tuned for text, not 4 KB binary chunks (though zlib still helps and structural sharing does most of the deduplication work anyway); and git tree objects impose filename semantics, so chunk references must be encoded as fan-out directories of hash-named blobs or kept out of trees entirely by referencing blobs directly from manifest content. There is also a hash-function decision: SHA-1 repositories are the interoperable default, SHA-256 repositories are cleaner but still second-class across hosting providers.

**Option B: the Dolt route.** Git *semantics* over a bespoke content-addressed chunk store, with a custom remote protocol. Optimal performance, total control over chunk format, journals and garbage collection — at the cost of rebuilding transport, hosting and every ecosystem affordance, which for a workbench product is a large fraction of the value proposition.

**Option C: hybrid.** A native chunk store for the hot working format, with git as the interchange and remote format — commits serialised into a git-repo layout on push and hydrated on pull.

**Recommendation: Option A, behind a `ChunkStore` abstraction, with Option C as the designed escape hatch.** The reasoning is scale-relative. An asset registry at workbench scale — call it 10⁴ to low 10⁷ nodes and edges — sits comfortably within git's performance envelope, particularly with proactive packing (write chunks straight into packfiles rather than loose objects, which gitoxide's pack-writing supports). Dolt chose Option B because it targets full MySQL-scale OLTP; acetone does not. Defining storage against a narrow trait (`get(hash) → bytes`, `put(bytes) → hash`, ref read/write, commit read/write) keeps the door open to a native store later without touching the tree, model or query layers. A pragmatic corollary: use git's own object hash as the prolly tree's content address, so there is exactly one addressing scheme in the system.

### Decision 2 — Node identity, the crux of merge

This is the single most important modelling decision, because everything Dolt gets right about diff and merge is downstream of one rule: rows have primary keys. Neo4j-style internal auto-increment IDs are merge-hostile — two branches that independently create "the same" asset would produce colliding or duplicate identities with no principled reconciliation, and diffs would be incomprehensible.

The recommendation is to make **natural keys mandatory**: every node carries a primary key composed of its (primary) label plus one or more key properties, declared in schema — `(:Host {hostname})`, `(:Supplier {companies_house_no})`, `(:Certificate {serial, issuer})`. Node identity in storage is `(label, key-tuple)`. Edges are identified by `(src-key, reltype, dst-key)` plus an optional discriminator property for parallel edges of the same type. This buys deterministic diffs, cell-wise merge, and — critically for the import workflow — *idempotent imports*: re-importing an unchanged CMDB export produces byte-identical trees (history independence) and therefore a detectable no-op.

For genuinely key-less data, offer surrogate ULIDs as an explicit opt-in with a documented caveat: surrogate-keyed nodes created concurrently on two branches merge as duplicates, not conflicts, and deduplication becomes the user's problem. An asset registry should almost never need this; the friction is a feature.

### Decision 3 — Encoding a property graph in ordered key–value maps

A prolly tree is an ordered map. Dolt's move is to make everything in the database a prolly tree and hash the roots together; acetone does the same with a graph-shaped decomposition. The working proposal is a small set of maps per graph, each an independent prolly tree whose root hash is recorded in the commit's manifest:

The **node map** keys `(label, key-tuple)` to a value record containing secondary labels and properties (CBOR or a FlexBuffers-style format — deterministic serialisation is mandatory or history independence dies). The **forward edge map** keys `(src, reltype, dst, disc)` to edge properties, and the **reverse edge map** keys `(dst, reltype, src, disc)` to nothing (or to the same record), giving O(log n) expansion in both directions — this is the graph-database equivalent of Kuzu's double-indexed adjacency, expressed as sorted maps rather than CSR. A **schema map** holds label definitions, key declarations, and (later) constraints and index definitions. Optional **property index maps** key `(label, property, value, node-key)` for equality/range seeks. Because every map is content-addressed, index maps are verifiably consistent with the data they index at every commit, and can be rebuilt deterministically.

The known cost, straight from Dolt's experience: write amplification (a one-property change rewrites a ~4 KB chunk path to the root of each affected map) and the impossibility of columnar layouts. Both are acceptable at workbench scale, and the second is mitigated at query time by projection pushdown rather than storage layout.

### Decision 4 — Merge semantics for graphs

Three-way merge proceeds map-by-map, exactly as in Dolt: keys added/removed/modified on one side only merge cleanly; keys modified on both sides recurse to property-wise (cell-wise) merge; same-property divergence is a conflict. Graphs add two genuinely graph-shaped complications on top. First, **dangling edges**: branch A deletes a node while branch B adds an edge touching it — each map merges cleanly in isolation, so referential integrity must be a post-merge validation pass that converts violations into structured conflicts. Second, **constraint violations**: merges can produce states violating uniqueness or cardinality constraints even when no individual key conflicts.

The proposal for conflict *representation* is one of acetone's nicer opportunities: conflicts are data, stored in a dedicated conflict map and exposed to Cypher as a queryable virtual subgraph (`MATCH (c:_Conflict)-[:_OURS]->(...)`), the graph analogue of Dolt's `dolt_conflicts` tables. Resolution is then just Cypher writes plus a `resolve` command. This also dry-runs the virtual-node machinery needed later.

### Decision 5 — Cypher engine strategy

Three paths: adopt ocg and swap its storage backend; transpile to something else; or build a pipeline on an existing parser. Transpilation has no sensible target here. Adopting ocg is tempting given its TCK record, but it is built around in-memory graphs with integer IDs and its execution model would fight the natural-key, prolly-tree-scan storage — the impedance mismatch likely costs more than it saves, though its TCK harness and its function library are directly reusable.

**Recommendation: own engine, borrowed front end.** Take decypher (or the openCypher grammar directly) for parsing; build a conventional pipeline of AST → logical plan → straightforward physical operators — label scan, key seek, index seek, expand (via the edge maps), filter, project, aggregate, sort/limit — in a Volcano-style iterator model over the storage traits. Run the openCypher TCK from day one and publish the conformance number; let it climb release by release rather than gating the MVP on completeness. Target subset for the MVP: `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH`, `ORDER BY`, `SKIP/LIMIT`, `UNWIND`, parameters, then `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, and variable-length paths. Vectorisation, factorisation and worst-case-optimal joins are explicitly *not* MVP concerns — correctness and TCK coverage are.

`MERGE` deserves a special note: with mandatory natural keys, `MERGE` on a key pattern becomes the canonical idempotent-import primitive, and its semantics align exactly with the storage model. That alignment is a small design gift.

### Decision 6 — The versioning surface

How does version control appear inside the query language and at the CLI? The CLI is the easy half — mirror git/Dolt verbs: `acetone init | clone | branch | checkout | merge | log | diff | status | commit | push | pull | import | query | shell`. The working set is a mutable root manifest held in a workspace ref (Dolt's WORKING/STAGED pattern), so edits accumulate and `commit` snapshots them.

Inside Cypher, three complementary mechanisms: a session-level checkout (query whatever the session has checked out — the workbench default); an `AT` addressing form for time travel (`MATCH (n:Host) AT 'main~5'` or a `USE asset_graph AT <ref>` prologue — final syntax to be settled against GQL's session/graph model so it doesn't collide with the standard's evolution); and procedures for history operations (`CALL acetone.log()`, `acetone.diff('main','import/2026-07')`, `acetone.blame(nodeKey)`). Diff results should themselves be graphs — added/removed/modified nodes and edges as a virtual subgraph — because "what changed between these two states of the estate?" is *the* asset-registry question and it deserves graph-shaped answers.

## Brainstorming — sketches beyond the decisions

**Import as commit discipline.** Every import runs as: transform source to canonical node/edge records deterministically, apply via bulk `MERGE`, commit with structured trailer metadata (source system, extractor version, source snapshot hash, timestamp). History independence then gives free change detection — an unchanged source yields an identical root and the import degenerates to a no-op. Scheduled imports (a natural Anthropic-Routines-shaped job) produce a commit-per-run audit trail, and `diff` between consecutive import commits *is* the change report. Conflicting concurrent imports land on branches and merge under Decision 4's semantics.

**Diff-as-graph and blame-as-graph.** Beyond the procedure interface, consider first-class virtual labels: `(:_Added)`, `(:_Removed)`, `(:_Modified {property, ours, theirs})` overlaid on the real graph for a given ref pair. Blame for a node is the sequence of commits whose diffs touch its key — cheap to compute by walking the commit graph and probing the node map path, since prolly trees make "did this key's path change?" an O(log n) question per commit.

**The review workflow.** Because commits are real git commits (Decision 1A), an asset-change proposal can literally be a branch pushed to GitHub, reviewed as a PR (the manifest diff is opaque, but CI can render `acetone diff` output as a comment), and merged by acetone on approval. Data change review — Dolt's flagship workflow — arrives nearly free.

**Virtual nodes, views, projections (future, but constraining now).** Store view definitions as named Cypher queries in the schema map, versioned like everything else. A materialised view is a derived set of maps whose manifest records the definition hash and source root hash — staleness is a hash comparison, refresh is deterministic recomputation, and the whole thing is audit-clean. RDF projection follows from natural keys: `(label, key)` tuples mint stable IRIs under a per-repo base, edges become predicates, properties become literals — oxigraph then gives SPARQL over an export or a live adapter. Relational projection maps each label with a declared key to a table, which is also the CSV/Parquet export story and, eventually, a SQL/PGQ story. None of this needs building now, but Decisions 2 and 3 were shaped so that all of it stays cheap.

**Classification and access (future).** Given per-map decomposition, a red/amber/green-style classification could partition sensitive properties into separate maps with separate visibility — noting that content-addressing means anyone with chunk access has data access, so real controls live at the repo/remote boundary, which git already handles.

## Risk register (design-stage)

The highest technical risk is **git object store performance under chunk-heavy workloads** — mitigated by the ChunkStore trait, pack-first writing, and an explicit Phase 0 benchmark gate before the architecture is locked. Second is **Cypher surface area**: the TCK is large and expression semantics (null handling above all) are notoriously fiddly — mitigated by TCK-first development and honest subset labelling. Third, **merge correctness at the graph level** (dangling edges, constraint interplay) has no off-the-shelf prior art and needs property-based testing from the start. Fourth, **ecosystem bets**: gitoxide's write-side gaps (fall back to git2 where needed) and the youth of the prollytree crate (treat as reference, not dependency, unless Phase 0 says otherwise). Finally, **GQL drift**: keep the parser layer swappable and avoid inventing syntax where GQL has an answer.

## References

Dolt storage engine and prolly trees: https://docs.dolthub.com/architecture/storage-engine and https://www.dolthub.com/docs/architecture/storage-engine/prolly-tree/ and https://www.dolthub.com/blog/2020-04-01-how-dolt-stores-table-data/ · Prolly trees vs columnar: https://www.dolthub.com/blog/2025-09-10-challenges-with-prolly-trees-and-columnar-storage/ · TerminusDB: https://terminusdb.org/docs/terminusdb-explanation/ · Kuzu and successors: https://gdotv.com/blog/kuzu-legacy-embedded-graph-database-landscape/ and https://www.cidrdb.org/cidr2023/papers/p48-jin.pdf · ocg: https://docs.rs/ocg · decypher: https://github.com/sunsided/decypher · prollytree crate: https://github.com/zhangfengcdt/prollytree · gitoxide: https://github.com/GitoxideLabs/gitoxide · openCypher/GQL: https://opencypher.org/ and ISO/IEC 39075:2024.
