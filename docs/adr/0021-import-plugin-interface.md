# ADR-0021: Import plugin interface, provenance and no-op detection

*Status: accepted — ratified by Greg at the Phase 5 boundary (2026-07-09) · Date: 2026-07-08 · Bead: acetone-6g5.1*

> **Phase 5 boundary outcome (2026-07-09).** Placement (extractors in the CLI)
> accepted. Authoritative-replace accepted for now, with the human-curation
> risk it carries — a re-import blats manual annotations on replaced nodes —
> tracked as `acetone-6g5.11`. The priority direction there is import-to-branch
> `+` merge (reuse the Phase 4 merge/conflict machinery), with a per-import
> `--protect <props>` list as a secondary fallback; deletions/full-sync remain
> further out.

## Context

Phase 5 opens with `acetone import <plugin> <source>` (spec §7): a plugin
interface for source-system extractors, CSV and JSON/NDJSON built in, a
deterministic transform to canonical node/edge records applied via bulk
`MERGE`, provenance carried in commit trailers (`Acetone-Source`,
`Acetone-Extractor`, `Acetone-Source-Hash` — anticipated in spec §3.5 and the
`NewCommit.trailers` doc comment), `--branch` import isolation, and detected
no-ops when an unchanged source is re-imported. This ADR records the shape of
that interface, where it lives, the new dependencies, and the semantics that a
fresh reviewer would otherwise have to reverse-engineer.

## Decision

### Placement — no new crate; interface in `acetone-graph`, format adapters in the CLI

The plugin trait, the canonical record model, the schema-driven transform and
the import orchestration live in a new `acetone-graph::import` module. Import
is mutation orchestration, which is `acetone-graph`'s charter, and keeping it
there honours the six-crate layout of spec §8 (no seventh crate). Crucially the
transform reads the target label's key tuple from `acetone-model` schema types
(`LabelDef::key()`) directly, so it needs **no** upward dependency on
`acetone-cypher`'s `Catalogue`; `acetone-graph` gains no new dependency at all.

The two built-in **format extractors** (`CsvExtractor`, `JsonExtractor` for
both JSON arrays and NDJSON) live in `acetone-cli`, the thin client where I/O
belongs. This confines the new third-party dependencies to the leaf crate:
- `csv` — the de-facto Rust CSV reader (BurntSushi, MIT/Unlicense, widely used);
- `serde_json` — the de-facto JSON parser (MIT/Apache-2.0);
- `sha2` — RustCrypto SHA-256 for the source hash (MIT/Apache-2.0).

Consequence: a library consumer of `acetone-graph` gets the *plugin interface*
(the `SourceExtractor` trait and orchestration) but writes or imports its own
extractors; the built-in file-format adapters are a CLI concern. This matches
the spec's framing — the library is the product surface; concrete file formats
are I/O adapters — and keeps the core dependency graph lean.

### The interface

```rust
pub trait SourceExtractor {
    fn name(&self) -> &str;                       // -> Acetone-Extractor trailer
    fn extract(&mut self) -> Result<Vec<ImportRecord>, ImportError>;
}

pub enum ImportRecord {
    Node { label: String, properties: BTreeMap<String, Value> },
    Edge { rtype: String, src: EndpointRef, dst: EndpointRef,
           discriminator: Value, properties: BTreeMap<String, Value> },
}
pub struct EndpointRef { pub label: String, pub key: Vec<Value> }
```

Extractors are **schema-agnostic**: they yield labelled property bags carrying
*all* fields (key and non-key alike). The schema-aware transform in `run` then
splits key properties out using `LabelDef::key()` and builds the canonical
`(NodeKey, NodeRecord)` — mirroring `acetone-cypher::persist::node_key_and_record`,
and preserving Invariant #3 (key properties MUST NOT appear in `NodeRecord`).

### Transform and type coercion

Each source field is coerced to the target label's declared `PropertyType`
when the schema declares one for that property name; otherwise it is kept as
the extractor's native value (a string from CSV; the native JSON scalar from
JSON). Coercion is total and deterministic (a fixed rule, not inference), so
re-importing identical bytes yields identical records and therefore an
identical manifest root (Invariant #1).

### Apply — authoritative replace, not property-merge

Records are applied with `Transaction::put_node` / `put_edge`, which *replace*
the whole record for a key. Import is therefore **authoritative**: the source
is the source of truth for the records it carries, and re-importing replaces
them wholesale. This is the standard sync semantic and the one under which
"unchanged source ⇒ no-op" holds exactly. It is deliberately **not** a
property-level merge that would retain fields from a previous, different source.
`put_edge` maintains `edges_rev` transactionally (Invariant preserved).
Endpoint nodes are **not** auto-created by edge import: an edge whose endpoints
are absent is a dangling edge, caught by `fsck`/merge validation, not by
import — import nodes before edges.

### No-op detection

After staging and `txn.save()`, `run` consults `Repository::is_dirty()` (the
existing workspace-manifest-vs-HEAD-manifest comparison). If the workspace
equals HEAD, the import made no change: `run` returns `ImportOutcome::NoChange`
and writes **no** commit. This is strictly more correct than comparing the
source hash to HEAD's trailer: it detects both an unchanged source *and* a
changed source that happens to produce an identical graph, and it does not
depend on HEAD having been an import at all. The source hash is still recorded
on real commits for provenance.

### Branch isolation

`--branch <name>` requires a clean workspace, records the current branch,
creates the branch (or checks it out if it already exists — so successive
scheduled imports append to the same branch), runs the import there, then
checks the **original** branch back out. The import commit lands on
`refs/heads/<name>` only; the caller's working branch is untouched. A clean
workspace is required for import in both modes so that provenance and no-op
detection describe the import alone, never pre-existing staged edits.

## Consequences

- End-to-end: `acetone import csv nodes.csv --label Host` produces a commit
  carrying the three `Acetone-*` trailers; re-running it on unchanged bytes is
  a reported no-op with no commit; `--branch` lands the work on a side branch
  and leaves the caller where they were; `fsck` stays clean.
- `acetone-graph` gains an `import` module but no new dependency; `csv`,
  `serde_json` and `sha2` enter the workspace for the first time, confined to
  `acetone-cli`.
- **Authoritative-replace** (not property-merge) is the load-bearing semantic
  choice; it is what makes no-op detection exact and is the natural fit for a
  scheduled source sync, but it means import overwrites, rather than augments,
  records it touches.
- Source hash is `sha256(raw source bytes)` in hex — independent of the repo's
  git object format, and the source blob itself is **not** stored.
- **Deferred / flagged for the Phase 5 boundary:** the authoritative-replace
  vs property-merge choice; confining built-in extractors to the CLI (vs a
  future `acetone-import` library crate if headless frontends need them);
  richer mapping (multiple labels per file, computed keys) beyond the
  `--label` / `--edge --from --to` surface.
