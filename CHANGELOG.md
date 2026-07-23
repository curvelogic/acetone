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

[Unreleased]: https://github.com/curvelogic/acetone/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/curvelogic/acetone/releases/tag/v0.3.0
[0.2.0]: https://github.com/curvelogic/acetone/releases/tag/v0.2.0
[0.1.1]: https://github.com/curvelogic/acetone/releases/tag/v0.1.1
[0.1.0]: https://github.com/curvelogic/acetone/releases/tag/v0.1.0
