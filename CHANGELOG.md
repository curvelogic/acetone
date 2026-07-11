# Changelog

All notable changes to acetone are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and acetone follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) from 0.1.

The section for each released version below **is** that GitHub release's notes
(the `Release` workflow reads it verbatim), so keep entries human-readable and
summarised: group related work, say what changed and why it matters, and leave
the commit-by-commit detail to git history. Add new entries under
`[Unreleased]` as work merges; move them under a new version heading when a
release is cut.

## [Unreleased]

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
- `query` for one-shot queries and an interactive `shell` REPL, both with table
  and JSON output.

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

[Unreleased]: https://github.com/curvelogic/acetone/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/curvelogic/acetone/releases/tag/v0.1.0
