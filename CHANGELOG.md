# Changelog

All notable changes to acetone are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and acetone follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) from 0.1.

The section for each released version below **is** that GitHub release's notes
(the `Release` workflow reads it verbatim), so keep entries human-readable and
summarised: group related work, say what changed and why it matters, and leave
the commit-by-commit detail to git history. Add new entries under
`[Unreleased]` as work merges; move them under a new version heading when a
release is cut. (One extractor caveat: don't begin an entry line with a
reference-style link definition — `[label]: url` at column 0 — as the release
workflow reads that as the end of the section. Inline `[text](url)` links are
fine.)

## [Unreleased]

### Added

- **`acetone serve --stdio`** — the frame protocol over stdin/stdout
  (the LSP child-process pattern; ADR-0076's companion feature): the
  host spawns the daemon and owns the pipe, so there is no socket path,
  no stale-socket reclaim and no kernel-ACL question. Stdout carries
  nothing but frames (no readiness line — the server-first hello is the
  readiness signal; logs go to stderr), and closing stdin is the
  graceful shutdown. Same verbs, same budgets; `--socket` and `--stdio`
  are mutually exclusive and one must be chosen.

- **Daemon `export` and `fsck` verbs** (Phase 12 verb parity): `export`
  streams a table — or, with neither `label` nor `edge`, every table,
  each announced by a `table` frame — as `chunk` frames rendered by the
  same code as the CLI's `acetone export`, with no path ever crossing
  the wire (the peer names its own files); `fsck` streams findings as
  frames and reports `{clean, errors, advisories}`. `serve --help` now
  describes the full verb set rather than the unit-1 build.
- **Graceful SIGTERM drain for `acetone serve`** (ADR-0074 §7's
  anticipated refinement): on SIGTERM the daemon stops accepting,
  unlinks its socket immediately (new connections fail fast), lets each
  in-flight request complete before its connection closes, waits up to a
  grace period (default 30 s, `ACETONE_SERVE_DRAIN_GRACE_SECS`) for
  handlers to finish, and exits 0. A second SIGTERM during the drain
  forces an immediate exit (nonzero).

### Changed

- **Breaking (daemon protocol):** the `status` verb's `ok` body is now
  exactly the document `acetone status --json` prints — it gains
  `schema_entries`, `workspace` (`"clean"`/`"dirty"`) and the
  `merge: {in_progress, conflicts_remaining}` block (null outside a
  merge), reports the branch by its short name (`main`, not
  `refs/heads/main`), and **drops the `dirty` boolean** in favour of
  `workspace`. One shape for both machine surfaces, rendered from one
  gathered set of facts, so a daemon embedding no longer reconstructs
  merge state by peeking git refs (acetone-sye1).

## [0.6.0] - 2026-08-11

### Added

- **`acetone serve`** — the per-repository daemon over a local `0600`
  unix domain socket (ADR-0074): versioned hello, length-prefixed JSON
  frames with a 16 MiB cap, the `query` verb (read and write — a
  write's terminal frame carries its summary counts, and a write may
  opt into relationship-type coinage with `params.autodeclare`) with
  streamed rows and the advisory channel, a read-only `status` verb,
  `schema-apply` and `import` verbs whose document/source streams in as
  `chunk` frames (no path ever crosses the wire, ADR-0074 §4 — a
  streamed import is staged to a daemon-private file the peer never
  names), and the `CALL acetone.log/conflicts/diff/blame` procedures
  reachable through `query`, the ref-advancing verbs `commit`, `branch`,
  `checkout`, `merge` and `resolve` (operating on the one shared
  per-worktree workspace — a connection is a view onto it, like
  concurrent CLI processes; per-connection isolated sessions are
  anticipated future work), **daemon-only stale-writer-lock recovery**
  (a write that hits a lock left by a SIGKILLed writer breaks it and
  retries once when the recorded pid names no live process — so
  `acetone serve` no longer crash-loops on a dead writer's lock; a live
  pid is always refused, never wrongly broken; ADR-0074 §8), writes
  serialising on the existing single-writer lock, per-query budgets unchanged, and a
  `--max-concurrent` bound (default 4) on their sum, a separate
  connection cap, read/write idle timeouts against slow/stalled peers,
  typed error kinds with span-aware rendering, and stale-socket reclaim
  so a host restart after a crash rebinds cleanly. A worked **non-Rust
  client** — `examples/acetone_daemon_client.py`, stdlib-only Python —
  drives an import/query/coin/schema-apply/status/commit/branch/merge
  session over the documented frame protocol, showing the daemon needs
  no acetone library to embed.
- **Multiple co-tenant graphs in one repository** (acetone-j6ui): a
  repository can now host several acetone graphs alongside its code,
  each on its own ref namespace and its own per-graph workspace, so
  their writes are isolated. `acetone init --co-tenant <name>` no
  longer refuses a second graph; a new global `--graph <name>` flag
  selects which one a command operates on (a repository with a single
  graph still needs no flag); `acetone graph list` shows the names; and
  a command that cannot choose says so, naming `--graph`. Library:
  `Repository::open_graph`, `Repository::list_graphs`,
  `fsck::check_path_graph`.

- **`acetone rename-rel-type OLD NEW`** — rename a relationship type,
  healing the near-duplicate predicates that autodeclare (ADR-0060)
  otherwise accumulates one-way (`was influenced by` → `influenced by`,
  ADR-0072 / acetone-lwv2). It rewrites every edge of OLD to NEW in one
  commit (both the forward and reverse maps, so `edges_rev` stays
  consistent) and moves the schema declaration. Renaming into a type
  that already exists is a **merge**: edges that would collapse onto an
  identical key are refused (naming them) unless `--merge` is given,
  which unions their properties and still refuses a property the two
  edges disagree on; irreconcilable declarations (differing
  discriminator regimes or property types) are refused. Workspace-level
  — past versions keep the old type. Library:
  `Repository::rename_rel_type`.

### Changed

- **A co-tenant graph's uncommitted workspace now lives on a per-graph
  ref** (acetone-j6ui): `refs/worktree/acetone/<graph>/workspace` and
  `.../<graph>/merge-head`, rather than the shared
  `refs/worktree/acetone/workspace`, so two graphs in one worktree no
  longer race a single workspace ref. Disposable per-worktree state, so
  no format bump: a graph written before this reads its workspace
  through a fallback to the old shared name, and the first write moves
  it to the per-graph ref (the pre-ADR-0014 migration path, extended).
  Standalone repositories are unchanged — they keep the global names.

### Fixed

- **fsck of a co-tenant graph now scopes to that graph's namespace**
  (acetone-j6ui): it walked `refs/heads/*`/`refs/tags/*`
  unconditionally, so in a repository shared with the user's own code
  (Phase 8 co-tenancy) it reported their non-acetone branch commits as
  findings. It now walks only the opened graph's
  `refs/heads/acetone/<graph>/` and tag namespace; `check_path`
  (the damaged-workspace entry) resolves the namespace the same way,
  falling back to standalone scope on an ambiguous marker.

## [0.5.0] - 2026-08-06

Acetone 0.5.0 is the "used in anger" release: the first project to build on acetone has begun doing so, and this release ships what that early use pulled — an **open vocabulary** (opt-in relationship-type autodeclare, declarative `schema apply`, surrogate `_id` minting, phrase-shaped type names) and **parallel relationships completed end-to-end** (declarable, importable, and now creatable and editable from Cypher), alongside the 0.4.x quality tier: stricter openCypher scoping (conformance 56.07% → 56.92%, still zero failures), bounded-and-cheap refusal of pathological queries, and per-record import memory bounds.

**Compatibility**: `format_version` stays **1** — repositories written by 0.1–0.4.x binaries are read and written unchanged. One data note: edges imported with `--disc` by earlier binaries may carry a stale record value under a property name that a schema later declares as the discriminator; reads now shadow such values (the key is the identity) and the next write heals them.

**Behaviour changes to note when upgrading**: the `schema --json` `relationship_types` shape changed from name strings to objects (the shape is explicitly unstable pre-1.0; every breaking change is CHANGELOG'd — now a recorded commitment in STABILITY.md); a typo'd property in Strict-mode `MATCH` now binds and returns 0 rows with a did-you-mean advisory instead of erroring (ADR-0070's open shape); `SET`/`DELETE` on discriminated edges now target the matched edge exactly (previously the wrong key — see Fixed); and library consumers: `BindError::UnknownProperty` is removed and `EvalCtx` is no longer struct-literal constructible.

### Added

- **Cypher-created parallel edges** (acetone-z093.5, ADR-0073, completing ADR-0030's reserved slot): when a relationship type declares a discriminator property, `CREATE`/`MERGE` resolve that property's value into the edge key — two creates with different values coexist; the same value is the usual duplicate refusal; MERGE matches per value. The discriminator is stored key-only and **re-exposed on read under its declared name, with the key winning any collision with a stale record value** (which the next write drops) — so `r.<disc>` also works on record-empty `import --disc` edges. `SET`, `REMOVE` and a whole-map `SET` that drop or change it are refused as identity changes, compared against the key; a declared discriminator absent from a `CREATE` map — or supplied as null, the unset-parameter trap — is refused: explicit identity, the node-key rule. Deferred-typed values (bytes, temporals) are refused from discriminators exactly as from node keys.

- **Relationship property types are now real** (acetone-7qw.12): `declare-rel-type` takes repeatable `--type <property>:<type>` flags, `acetone schema` renders them (JSON note: `relationship_types` entries are now objects `{name, discriminator, types, required}` rather than bare name strings — the `--json` shape is unstable pre-1.0), and declared types are enforced exactly as node ones are — on write, at declare time against existing relationships, re-validated at merge (a new `rel-wrong-type` conflict row kind in `CALL acetone.conflicts()`, with the relationship type in the `label` column), and checked by `fsck` as an advisory. Previously a relationship property type was declarable through the library, stored, and silently meaningless. A discriminator-named property is judged in both stored positions.

- **Relationship-type autodeclare, strictly opt-in** (ADR-0060, acetone-nc91): `acetone query --autodeclare`, the shell's `:autodeclare on|off`, and the library's `Session::autodeclare(bool)` let a write coin an unknown relationship type in `CREATE`/`MERGE` position — a deterministic empty definition appended to the schema in the same transaction as the data, announced by an advisory. Off by default; reads never coin a type; while a merge is unresolved a coining write is refused (it would otherwise silently resolve a schema conflict by-write). Convergent coinage of the same type on two branches merges cleanly.

- **`acetone schema apply`** (acetone-yx1o.1): consume the `schema --json` document declaratively — diff against the current schema, report per-entry outcomes, stage additions and changes in one transaction (a declare-time refusal rejects the whole document), never remove, idempotent on re-apply; `--dry-run` prints the plan. Within an entry the document is desired state (omitting a facet drops it — the plan says so); across entries apply never removes. Also the first CLI surface that can declare a surrogate label (`"surrogate": true`). Hand-edited documents are guarded: unknown fields, unknown type names, and duplicate JSON keys within one object are refused, and `apply` refuses outright while a merge is unresolved (it would otherwise silently resolve conflicted schema entries).
- **Surrogate `_id` minting** (spec §2, acetone-yx1o.4): `CREATE` on a `KEY SURROGATE` label mints a ULID `_id` at creation, visible to the creating query's rows; an explicit `_id` is respected; `MERGE` matches before minting again. Natural-key labels are untouched.

### Changed

- **ORDER BY and aggregation scoping now enforce the openCypher grouping-key rules** (acetone-1qj): an aggregate in ORDER BY after a non-aggregating projection, an ORDER BY reference that does not reduce to projected grouping keys/aliases after DISTINCT or aggregation, and a projection item mixing an aggregate with anything but simple projected grouping keys are now compile-time errors (`InvalidAggregation`, `UndefinedVariable`, `AmbiguousAggregationExpression`) instead of over-accepted. Published TCK pass rate rises 2185 → 2218 of 3897 (56.07% → 56.92%), still with zero failures.
- **Changing a relationship type's discriminator while relationships of that type exist is now refused** (`GraphError::RelDiscriminatorChanged`) — including the silent wipe a definition-replacing redeclare performed: a library caller that redeclared a discriminated relationship type over live relationships previously succeeded and dropped the discriminator; it now errors and requires an explicit `migrate`.
- **Declared property types no longer close a label's shape** (ADR-0070). Previously, declaring any property type made node-pattern map literals reject undeclared property names (`CREATE (:Host {…, ip: …})` failed `unknown property`) while `SET` accepted them. Now undeclared properties are legal on every path, symmetrically; on a typed label they produce a stderr typo advisory (with did-you-mean) instead of an error. Note the flip side: a Strict-mode `MATCH (h:Host {typo_prop: v})` that previously errored now binds, matches nothing, and advises — check stderr when a query unexpectedly returns 0 rows. Type *enforcement* on declared properties (ADR-0066) is unchanged.

- `acetone query` and `acetone shell` now arm a **60-second wall-clock budget by default** (each takes `--timeout <seconds>` to change it, `0` to disable; a cut-off query fails with a typed error naming the flag). The deterministic work caps are unchanged and still apply; the timeout bounds how long they may take to be reached on a store-backed graph (ADR-0069). The library's `QueryLimits::default()` is untouched — embeddings stay deterministic unless they opt in.
- Deep-access API: `acetone-cypher`'s `EvalCtx` gained a private cache field, so it can no longer be constructed by struct literal outside the crate (use `EvalCtx::new`); `BindError::UnknownProperty` is removed (no longer produced — ADR-0070) and `BoundQuery` gained the public `undeclared_shape_properties` field. The curated `acetone-core` surface is unaffected.

### Fixed

- **The import UNIQUE-violation path is no longer quadratic** (acetone-7qw.2, Phase 9 security review): violation reporting re-scanned every interned unique value per violation; an inverse claim-key index makes reconstruction O(1) per violation (measured 6.10 s → 0.72 s at 400k workspace values).
- **CSV/NDJSON import memory is now bounded per record** (acetone-7qw.4, Phase 9 security review): a single pathological record — a newline-less multi-gigabyte NDJSON file, one huge quoted CSV field — previously allocated its whole size despite ADR-0062's bounded-memory promise. A 64 MiB per-record cap now refuses with a typed error (`--format json` still whole-parses, per the ADR's recorded residual). And a schema declaring more than 65536 distinct UNIQUE (label, property) pairs now yields a typed import error instead of panicking the process (acetone-7qw.3).

- **Item-wise Cypher edits on discriminated (parallel) edges targeted the wrong edge** (acetone-z093.4, the o8r hazard, live since `import --disc` in 0.1): `MATCH` binds an edge by its full key — including the discriminator — but `SET`/`DELETE` recomputed identity with a `Null` discriminator. Shipped symptoms in 0.4.0: `DELETE` reported deletions that did not happen; `SET` silently minted phantom `Null`-key edges (`fsck`-clean corruption); `DETACH DELETE` of a node with discriminated edges failed outright with a dangling-relationship error; and a delete-plus-create in one statement could silently overwrite an unrelated edge's record. All four now use the bound edge's exact key. Cypher `CREATE` still writes a `Null` discriminator — the create side is acetone-z093.5.

- With several usable index hints on one pattern, the planner now sizes every alternative (candidate counts only — no point reads) and materialises the smallest, instead of taking the first that fits its budget: an under-cap unselective equality no longer beats a far more selective range on the same pattern (previously measured 65× off the best available plan). Sources that cannot size keep the previous serve-order behaviour. Two knock-ons worth knowing: sizing is unmetered enumeration work (bounded per probe by the cost model's candidate cap), and a store read error on a *losing* probe's index can now surface on a query that previously never touched that index — stricter, not looser.
- The chained-comparison expansion budget now counts string payload bytes as well as expression nodes, so a long string literal duplicated by a comparison chain is refused up front rather than admitted at ~1/780th of its real allocation weight (a 4 MB query could transiently allocate 3.1 GB inside the old cap). Long strings outside chains are unaffected.
- The governed scan pathology (a fresh anchor in a pattern comprehension or pattern predicate, re-evaluated per row) is now refused in seconds rather than minutes: label-scan materialisations and expansion probes are memoised per evaluation context, while the governor's deterministic charges stay byte-identical — limits trip at exactly the same point as before (measured on the shipped CLI against a 20k-node store-backed repository: 702.9 s → 9.9 s to the typed refusal). Both memos' retention is capped (1M cached tuples for expansion probes, 1M nodes for label scans — the latter added by the phase's milestone security review, which measured 2.36 GB retained from a 2.5 KB query before the cap); past a cap, scans and probes still run and still charge, they just stop being retained.
- The ORDER BY/aggregation grouping-key validation runs in linear time on large queries (a structural-digest index replaces a probe that measured quadratic — 32.8 s of bind time at 408 KB of query text; found by the milestone security review, which also noted binding runs before the wall-clock governor and so was otherwise uncovered).

- The public-API freeze gate now signature-tracks the library crates behind the façade (`acetone-graph`, `acetone-model`, `acetone-store` and `acetone-prolly` join `acetone-cypher` as full-signature snapshots, alongside `acetone-core`'s re-export list), closing the blind spot the 0.4.0 notes described: a shape, attribute or method change to a re-exported type now fails CI in the crate that hosts it, wherever it lives. See STABILITY.md.

## [0.4.0] - 2026-08-01

**At scale, and in conformance** (Phase 9). The query engine stops declining
large parts of openCypher, and the storage and verification paths stop
assuming everything fits in memory or in one pass. openCypher TCK conformance
rises to **2185 / 3897 (56.07%) with zero failures**, from 1602 (41.1%) at
0.3.1; every residual parse rejection is individually enumerated and justified
in `docs/conformance.md`.

Indexing becomes *beneficial* rather than merely present: seeks now reach the
shipped read path and win outright where they should — 17× on a
0.27%-selective range and ~600× proving a bucket empty, both against an
identical unindexed twin repository; 1104× on a primary-key lookup, against
the label scan it replaces (the twin cannot show that one, since both twins
declare the same key and so run the same plan). All measured through
`Session` at 110,200 nodes. Where a seek would lose, it now **declines to the
scan** — the regime that at that scale ran up to 37× *slower* than no index
before ADR-0065.

A minor bump, so pre-1.0 it carries deliberate breaking changes: three frozen
types are now `#[non_exhaustive]`, and a declared property type is enforced
where it previously was not. Both are called out below. No on-disk format
change — `format_version` stays 1 and 0.1–0.3.x repositories are read and
written unchanged.

### Added

- **Query-language coverage**: pattern comprehensions
  (`[ (a)-[r]->(b) WHERE p | expr ]`), label predicates in expression
  position (`WHERE n:Label`), chained comparisons (`1 < 2 < 3`),
  `CALL … YIELD` aliasing and `YIELD *`, and bidirectional relationship
  patterns.
- **Streaming import** (ADR-0062): a source larger than memory imports in
  bounded resident memory — records are pulled one at a time and staged in
  batches (`--batch-size`, default 8192). The bound is unconditional for
  UNIQUE-free imports; with unique-constrained labels the tracker is
  compact but grows with those labels.
- **Index range seeks and primary-key point lookups** reach the shipped
  read path: inequality/range predicates on an indexed property, composite
  index seeks, and `KeySeek` on a label's declared key are planned and
  served through `Session`, declining to a scan when unselective (see the
  cost model under *Changed*).
- **Index-backed UNIQUE enforcement**, including uniqueness within a single
  statement's writes, keyed by the memcomparable value encoding (so
  `0.0`/`-0.0` and NaN behave correctly).
- **`fsck` anchor-completeness checks** (ADR-0063) for commits and
  workspace refs — the "clean now, data gone after `git gc` later" class is
  reported while it is still recoverable, including for pre-ADR-0014 legacy
  workspaces.
- **Worktree-aware `gc`** and a streaming `fsck` canonical-map rebuild, so
  both operate on repositories whose node maps exceed memory.
- An **advisory when a query names an undeclared label** in expression
  position, catching typo'd labels that would otherwise just return no rows.
- **`declare-label --type <property>:<type>`** — property types are
  declarable through the CLI, taking `bool`, `int`, `float`, `string`,
  `bytes`, `date`, `time`, `datetime`, `duration` and `list`. `acetone
  schema` renders what is declared, in both the text and `--json` forms.
  They were previously reachable only from the library.

### Changed

- **`WITH … WHERE` now filters after `SKIP`/`LIMIT`**, matching
  openCypher's sub-clause order. Queries combining them can return
  different (now-conformant) results, and an `ORDER BY` key error can
  surface on rows the `WHERE` would previously have discarded first.
- **Aggregates inside comprehension, quantifier and `reduce` bodies are
  compile-time errors** (`InvalidAggregation`) rather than silently wrong
  answers; aggregate slots are keyed by expression identity, fixing wrong
  values when an aggregate sat in a skipped `CASE` branch.
- **`merge_base` runs in two linear walks** (paint-down maximal-common
  ancestors), flat across histories that previously grew its cost
  quadratically.
- **Parse-time resource bounds**: a chained-comparison desugar bomb and
  allocation-amplifying query shapes are refused in milliseconds at
  megabytes (previously unbounded before any governor existed), and anchor
  scans in pattern comprehensions/predicates are charged against a
  dedicated scanned-candidate budget (ADR-0064) with a typed
  `ResourceExceeded` error.
- **A declared property type is now a constraint, not an annotation**
  (ADR-0066). It is enforced when a transaction saves, when a type is newly
  declared or changed over existing data (which is refused, naming a
  violating node — `declare-label` still names them all), and at merge —
  where a breach is reported as a conflict rather than an error. `float`
  admits an integer, matching what import already did; nothing else widens.
  This is a deliberate spec change (§2): writes that contradicted a
  declaration were previously accepted, and were already getting wrong
  answers from any seek over that property. No on-disk format change.

  **Library consumers: `Transaction::save`/`commit` can now fail where they
  previously succeeded.** Installing a declaration that the data already
  present contradicts is refused, not only through `declare-label` but
  through `put_schema` on any transaction. Nodes the same transaction
  rewrites or deletes are excluded, so a retype and its backfill still land
  together. No signature changed; both already returned `Result`.
- **An unfinished merge no longer trusts its own declared types.** A merge
  that produces graph-level violations persists the merged manifest as the
  workspace and rebuilds indexes over it; the commit is refused, but that
  workspace stays queryable, and in it one branch's declaration can sit
  beside the other's contradicting data. A seek relying on the declaration
  could then return fewer rows than the equivalent scan. Until the merge is
  completed or aborted, seeks that would depend on a declared type fall back
  to a scan — correct, and slower only for string pins. This holds for the
  whole merge, including after conflicts have been resolved but before the
  merge commit lands.
- **A primary-key point lookup is served by a seek.** `MATCH (h:Host
  {hostname: "db1"})` previously scanned every node of the label, because
  a string key pin could not rule out a `bytes`/temporal value equal to
  the pin's text rendering. It now consults the key property's declared
  type, exactly as an equality seek does — so typing your key properties
  is what buys the lookup. On a 110,200-node graph: 240.9 ms to 0.25 ms.
- **Indexes are now chosen by estimated cost** (ADR-0065). A seek does one
  random point read per matching row where a scan reads sequentially, so a
  seek only wins while selective. Both the equality/composite and range
  paths now estimate what the scan would cost, spend a fixed fraction of it,
  and otherwise decline to the scan. The cliff this removes was severe:
  cases measured at 53x, 18x and 12.5x slower than no index at all now run
  within 1.04-1.23x of a scan, while selective queries gain outright
  (0.16-0.24x). The residual is a constant — a declining seek has still paid
  for its index probe and one cardinality sample — so it is only negligible
  relative to a scan worth avoiding: within ~1% at 110,200 nodes, but
  1.2-2x on a graph small enough for the whole scan to take a millisecond.
- **`WHERE` equality predicates use indexes.** `MATCH (n:L) WHERE n.p = 1`
  previously scanned; only the inline form `MATCH (n:L {p: 1})` used an
  index. Seek hints now carry their own pinned values, so both forms plan
  the same way.
- Seek hints are an ordered candidate list rather than a single choice, so a
  hint the cost model declines at runtime falls through to the next instead
  of discarding a usable plan.
- **BREAKING (library): `GraphError` is now `#[non_exhaustive]`.** A downstream
  `match` over it must carry a wildcard arm; add one and every future variant
  becomes a non-event for your build. This is a one-time break taken
  deliberately at a minor boundary, because the alternative is breaking
  consumers every time the repository layer grows an error (`NothingToCommit`
  in 0.3.1, `PropertyTypeViolation` in this release).
- **BREAKING (library): `QueryLimits` and `ResourceLimit` are now
  `#[non_exhaustive]` too**, and `QueryLimits` gains chainable setters —
  `QueryLimits::default().with_max_result_rows(1_000)` — because a
  non-exhaustive struct cannot be built from another crate even with
  `..Default::default()`. Both types grow whenever the governor grows a cap
  (this release added `max_scanned_candidates` / `ScannedCandidates` with
  ADR-0064, itself source-breaking), so closing them now makes every future
  cap a non-event. Read access to the fields is unchanged.
- **Note on the public-API gate.** It caught the `acetone-cypher` changes
  above (that snapshot is signature-tracked) but is blind to the `GraphError`
  one: types re-exported from `acetone-graph`/`-model`/`-store` appear in the
  core snapshot by name only, so a change to their internals re-blesses to an
  empty diff (`acetone-7qw.5`). Hence the manual notes here. See STABILITY.md.

### Fixed

- `UNWIND` streams into a following `LIMIT` instead of tripping the
  result-row governor, and bound relationship-list var-length patterns no
  longer over-match.

## [0.3.1] - 2026-07-24

A **quality, security, and documentation** release. No new headline capability
and no on-disk format change (`format_version 1`; 0.1–0.3.0 repositories are
read and written unchanged) — this release hardens the workbench against
hostile input, closes constraint-enforcement and terminal-spoofing gaps found
by dogfooding, ships a complete operator's manual, and automates the release
path. openCypher TCK conformance rises to **1602 / 3897 (41.1%)** from 1596 at 0.3.0.

### Added

- **The acetone operator's manual** — an mdBook (`docs/manual/`, published to
  GitHub Pages) covering installation, a worked asset-registry example, a
  Cypher query cookbook, importing, history/branch/merge, schema and indexes,
  maintenance and migration, a recovery runbook, and a library/CLI reference.
  Every command and output in it is driven against the real CLI, and a CI job
  (`docs/manual/verify.sh`) plus link-checking keep the examples honest.
- **Query parameters on the CLI**: `acetone query --param KEY=VALUE`
  (repeatable) binds `$KEY`; VALUE is parsed as a Cypher literal — number,
  quoted string, `true`/`false`, `null`, or a list/map of literals — so
  quoting and typing match the language, and a bare unquoted word errors
  rather than silently binding a string. The shell gains `:param`/
  `:param-clear`, `--param` composes with `--at`, and the library gains
  `Session::query_at_with` and `acetone_cypher::parse_literal`.
- **`acetone log --all`** walks every branch tip, not just the first-parent
  chain, so a merged-in branch's commits are visible; merge commits show their
  parents on a structural line that repository-controlled message content
  cannot forge. Default `log` output is unchanged.
- **`acetone branch NAME [REFSPEC]`** creates a branch at an arbitrary start
  point (commit, branch or tag), and **`acetone branch --delete NAME`** removes
  one (refusing the checked-out branch), so branch recovery no longer needs raw
  `git update-ref`.
- **`acetone commit --allow-empty`** (and library
  `Transaction::commit_allow_empty`): deliberately record a commit with no
  content change — a marker commit — now that plain `commit` refuses one
  (ADR-0056).
- **Streaming counts**: `Snapshot::node_count`/`edge_count`/
  `schema_entry_count` count without materialising records; `acetone status`
  uses them, so status stays cheap on large graphs.
- **Release automation**: publishing a release now triggers a workflow that
  opens the Homebrew-tap formula-bump PR automatically, and each release
  archive carries a signed SLSA build-provenance attestation
  (`gh attestation verify …`). The release flow is also encoded as a tracked
  beads formula (`.beads/formulas/release.formula.toml`, ADR-0057).

### Changed

- **Constraints are enforced on every write surface.** `import`, `put-node`,
  and `declare-label` (retrofitting `--require`/`--unique` over existing data)
  now run the same existence and UNIQUE checks as a Cypher write, failing
  atomically and naming the offending nodes, and `fsck` reports pre-existing
  breaches as advisories. Previously `import` and `put-node` could commit a
  node a Cypher `CREATE` would reject.
- **`AT`/`--at` resolve short tag names and peel annotated tags** with
  git-parity precedence (exact ref path → tag → branch → commit hash), so time
  travel to a tag works the way `log`/`fsck` already did.
- **Graph-level merge violations surface through the whole resolution flow**
  (ADR-0058): while a merge is in progress and every cell conflict is
  resolved, `Repository::conflicts()` re-derives graph violations (dangling
  edge, missing required property, UNIQUE collision) live over the resolved
  workspace, so a violation the merge composed — or one a resolution
  introduced — is visible before commit refuses it: `acetone resolve` warns
  about violations it leaves, `status` counts them, and the merge-completion
  refusal now **names each violation** (`GraphError::MergeViolations`)
  instead of refusing anonymously. `CALL acetone.conflicts()` gains a leading
  `kind` column (`cell` | `dangling-edge` | `missing-required` | `unique`)
  and yields one row per violation. Library note: `PersistedConflict` is
  renamed `WorkspaceConflict`, whose `Graph` variant now carries the
  violation record.
- **Repository lifecycle hardening** (ADR-0056): `Transaction::commit` now
  refuses a commit that would record no change
  (`GraphError::NothingToCommit`) — merge completions are exempt, and the
  guard now lives in the library rather than as a CLI-side check; an
  interrupted `checkout` (crash between its two ref updates) is recovered by
  simply re-running the same checkout, and the update ordering is a
  documented contract; `Repository::open` is now strictly read-only — a
  fresh `git worktree add` worktree reads its checked-out commit directly
  and gains its workspace ref on first write, so read-only commands work on
  read-only filesystems and never contend with a writer.
- **`migrate` rewrites annotated tags** onto the rewritten history and swings
  every ref (branches, tags, workspace) in a single journalled, crash-safe
  transaction that a re-run completes; signed tags are refused rather than
  silently invalidated.
- **Clearer errors and cleaner output**: an undeclared-label error now suggests
  `declare-label`; write-only queries no longer print a spurious `(no columns)`
  line; map projections, out-of-range integer literals, and blame key-arity
  mismatches get actionable messages; and a duplicated error cause on lock/init
  failures is fixed.

### Fixed

- **Denial-of-service via deeply nested runtime values**: a query building a
  200 000-deep value with `reduce` (then `DISTINCT`/`ORDER BY`/grouping) aborted
  the process with a stack overflow. Runtime value construction is now bounded
  (`ResourceLimit::ValueDepth`), and query parameters are bounded at ingestion.
- **Executor resource accounting**: variable-length expansion, aggregation
  grouping, and `replace()` string amplification are now charged against the
  work/collection budget, and CBOR array/map preallocation is capped — closing
  memory-amplification and expansion-blow-up vectors.
- **`gc` hardening**: a crafted pack-sidecar stem could delete files outside
  the object directory (path traversal, now validated); a co-tenant graph could
  claim another graph's refs (now an explicit ownership allow-list); a graph
  name from a hand-crafted marker is revalidated on open; and a TOCTOU against a
  concurrent `git worktree add` is closed by re-checking under the writer lock.
- **fsck** now peels annotated tags and follows symbolic refs (rather than
  aborting), and dedups shared chunk sets across history so deep repositories
  are not re-walked per commit.
- **Cypher lexer** accepts the `i64::MIN` literal and its hex/octal forms; the
  `SET x = <entity>` and `MERGE … ON CREATE`/`ON MATCH` gaps the TCK pins are
  closed (+4 scenarios: 1598→1602).

### Security

- **Terminal spoofing**: zero-width and invisible Unicode characters in
  identifier-shaped output (labels, keys, relationship types, branch names,
  including identifiers projected into query result cells) are now escaped,
  completing the bidirectional-override defence shipped in 0.1.1; property
  values keep legitimate emoji sequences.
- **Persistence guard**: values that do not round-trip through query semantics
  (bytes and temporals) are rejected as node-key properties, so node identity
  can never diverge from comparison semantics.
- A milestone security review over the whole release diff accompanies this
  release; see `docs/reports/phase-0.3.1.md`.


## [0.3.0] - 2026-07-23

A **co-tenancy** release: an acetone graph can now live inside an ordinary git
code repository — its own refs alongside the code's history in one object
store — with the destructive operations provably staying in the graph's lane.

No on-disk format change: the format stays at `format_version 1`, and 0.1/0.2
repositories are read and written unchanged.

### Added

- **Co-tenant mode** (ADR-0049/0050): `acetone init --co-tenant <graph>`
  initialises a graph inside an existing code repository. Graph branches live
  under `refs/heads/acetone/<graph>/*`, graph tags under
  `refs/tags/acetone/<graph>/*`, and the graph's current-branch pointer at a
  local-only symref — the user's code branches and git `HEAD` are never
  touched. Co-tenant repositories are detected on open via an on-disk marker;
  standalone repositories behave exactly as before, byte for byte.
- **Format evolution machinery** (ADR-0048/0052): manifest decoding now
  dispatches on the stored `format_version` to retained per-version decoders
  (read-old-write-new). A future format bump will leave old commits readable
  in place — no history rewrite, no force-push — which is what makes a format
  change safe for a graph sharing a repository with code. The rewrite-based
  `migrate` remains available as a deliberate opt-in for standalone
  repositories.

### Changed

- **`gc` is graph-scoped** (ADR-0051, reading B): consolidation packs only the
  objects reachable from the graph's refs, with an explicit guard so nothing
  reachable from a non-graph ref (including `refs/remotes/*` in clones) is
  ever repacked or pruned — the user's code storage is left exactly as git had
  it. Tests prove a code-only object survives `gc` untouched and code refs
  survive `migrate`.
- **Consolidation packs are `.keep`-marked** (ADR-0053), so a foreign
  `git gc`/`git repack` — including git's automatic `gc.auto` — leaves
  acetone's content-aware deltas intact. Proven against the real
  `git repack -a -d`.
- `merge()` on a detached HEAD now reports `NoCurrentBranch` before
  `DirtyWorkspace`, matching the actual precondition failure. Co-tenant init
  refuses to layer a graph onto a repository that already holds a legacy
  (pre-workspace) standalone acetone graph, rather than misbehaving later.

## [0.2.0] - 2026-07-20

### Changed

- **The `acetone-core` library API is frozen** (ADR-0046). The curated headline
  surface re-exported at the crate root now follows semantic versioning —
  additive-only within 0.2.x, breaking changes require 0.3 — and is guarded
  against silent drift by committed public-API snapshots checked in CI (the API
  analogue of the format goldens). `QueryLimits`, `QueryResult`, `ResourceLimit`
  and `QueryValue` (the query result/parameter value type) are now re-exported
  at the crate root, completing the query surface. The whole-crate module
  re-exports remain available as *unstable* deep access. See `STABILITY.md`.

  No on-disk format change: `format_version 1` repositories are read and written
  unchanged.

## [0.1.1] - 2026-07-14

A CLI and Cypher **ergonomics** release — no on-disk format change, so 0.1.0
repositories are read and written unchanged. It makes the workbench pleasant
to drive by hand and gives error messages the same discipline the storage
engine already had. Every user-facing wording change is now pinned by
snapshot tests, so it can't silently regress.

**Clearer, actionable errors.** Node keys render readably (`Person [alice]`)
instead of leaking Rust internals; every Cypher error carries a `line L,
column C:` location (execution errors gained it — the byte-offset noise is
gone); unknown labels, properties, functions and relationship types suggest
the nearest declared name (`did you mean "hostname"?`); a bare `(Topic {…})`
or `[LINK]` explains the missing colon; and `DuplicateKey` gives the correct
MERGE idiom instead of misadvising. All attacker-writable text reaching the
terminal is escaped — including the bidirectional "Trojan-source" control
characters, so a hostile clone's labels, values or branch names can't visually
reorder your terminal.

**A CLI that reads like one.** `acetone --help` is grouped by role (everyday /
schema / data & query / maintenance / plumbing) with a note on how each
command relates to git; `cypher` is an alias for `query`; unique command
prefixes resolve (`acetone st` → status); `import`/`export` take a consistent
`--format` flag; and `acetone` from a **subdirectory** now finds the
enclosing repository (like `git -C`), preserving the config-isolation
boundary.

**See and script the graph.** New `acetone schema [--at <ref>]` displays the
declared labels, keys, relationship types and indexes for any version. A
`--json` flag on `status`, `log`, `branch`, `diff`, `list-nodes`, `get-node`
and `schema` makes the read commands scriptable (the JSON shape is not yet
frozen — it may change before 0.2).

**A real shell.** `acetone shell` now has readline line editing, history and
recall; a branch-aware prompt with a dirty marker; in-shell `:declare-*`,
`:commit`, `:status` and `:schema`; wide-character-aware table alignment; and
errors routed to stderr. It stays scriptable when input is piped.

## [0.1.0] - 2026-07-11

First release. Acetone is a **solo, git-native workbench for a
version-controlled asset registry**: a labelled property graph stored as
Dolt-style prolly trees inside an ordinary git object database, queried with
openCypher and driven from a single-binary CLI. Imports become audited commits,
diffs become change reports, and any git remote is backup and transport.

### Storage engine and data model

- **History-independent prolly trees** over the git object store: identical
  graph contents always yield identical tree hashes regardless of the order of
  operations that built them.
- **Deterministic encodings** — memcomparable keys (byte order equals logical
  order) and canonical CBOR values. The on-disk format is frozen at
  `format_version = 1` and golden-pinned; any change bumps the version.
- **Natural-key node identity** — a node is identified by its (primary label,
  key tuple), declared in the schema; key properties are immutable and `SET`
  can never change them.
- **Reproducible derived maps** — reverse edges and secondary indexes are
  rebuilt bit-for-bit from their sources (`reindex` yields identical roots).
- `acetone` graphs *are* git commits: `git log`, `git push`, `git clone` on the
  enclosing repository work untouched.

### Query and editing (openCypher)

- **Read path**: `MATCH` / `OPTIONAL MATCH` / `WHERE` / `RETURN` / `WITH` /
  `UNWIND`, aggregation, `ORDER BY` / `SKIP` / `LIMIT`, parameters, variable-
  length paths, and openCypher null semantics; time travel with `AT <ref>`.
  Published openCypher TCK conformance: **41.0% (1596 / 3897 scenarios)**, with
  the known gaps tracked.
- **Write path**: `CREATE`, `MERGE` (upsert on key), `SET`, `REMOVE`, `DELETE`,
  `DETACH DELETE`, batched into workspace commits. `MERGE`-based re-imports are
  idempotent — re-loading identical data leaves the root unchanged and `commit`
  reports nothing to commit.
- `query` for one-shot queries and an interactive `shell` REPL, both with table,
  JSON and CSV output.

### Versioning, diff and merge

- **Graph-level `diff`** as classified node/edge change streams and an
  `_Added`/`_Removed`/`_Modified` virtual graph.
- **Three-way merge** over the git commit graph: a pure, deterministic function
  whose conflicts are *data* (a queryable `conflicts` map), not errors —
  inspected and resolved in Cypher, recorded as merge commits.
- **Referential integrity** enforced at the transaction boundary, and **node
  blame** over history.

### Import, export and indexes

- **Import** from CSV and JSON/NDJSON with provenance trailers and `--branch`
  isolation, and no-op detection on unchanged snapshots so a scheduled import
  only commits real change.
- **Export** to CSV / JSON / NDJSON with round-trip fidelity.
- **Declared property indexes** with index-accelerated seeks and `reindex`.

### Operations and tooling

- `init`, `status`, `commit`, `log`, `branch`, `checkout`, `merge`, `diff`,
  `resolve`, `import`, `export`, `reindex`, `fsck`, `gc`, `migrate`, plus
  low-level plumbing.
- **`fsck`** verifies structural integrity (including index and reverse-edge
  consistency); **`gc`** reclaims unreachable objects idempotently and safely;
  **`migrate`** rewrites history under the `format_version` machinery.
- `#![forbid(unsafe_code)]` across the shipping surface.

### Packaging

- Distributed as a **single binary** — statically linked against musl on Linux,
  the platform binary on macOS — via GitHub Releases and a Homebrew tap.
- The library crates are **internal** for 0.1: no crates.io publication and no
  frozen public API. `acetone-core` is the intended library surface and
  stabilises at 0.2, gated on the query-engine resource governor.

The authoritative design record — data model, storage, encodings, query
language, diff/merge, and the phased roadmap — lives in `docs/`.

[Unreleased]: https://github.com/curvelogic/acetone/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/curvelogic/acetone/releases/tag/v0.5.0
[0.4.0]: https://github.com/curvelogic/acetone/releases/tag/v0.4.0
[0.3.1]: https://github.com/curvelogic/acetone/releases/tag/v0.3.1
[0.3.0]: https://github.com/curvelogic/acetone/releases/tag/v0.3.0
[0.2.0]: https://github.com/curvelogic/acetone/releases/tag/v0.2.0
[0.1.1]: https://github.com/curvelogic/acetone/releases/tag/v0.1.1
[0.1.0]: https://github.com/curvelogic/acetone/releases/tag/v0.1.0
