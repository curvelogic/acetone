# acetone

An embedded, single-node, **version-controlled labelled property graph
database**: Dolt-style [prolly trees][prolly] stored in a git-compatible object
store, queried with [openCypher][opencypher], operated as a workbench (a CLI and
a Rust library, not a server). Written entirely in Rust.

Branch, diff, merge and time-travel a property graph the way you branch, diff,
merge and time-travel code — history is a git commit graph, push/pull/clone are
git push/pull/clone, and a remote need not know acetone exists.

> Status: pre-0.1, hardening. The on-disk format is frozen at
> `format_version = 1` (see [ADR-0024](docs/adr/0024-gate-d-format-freeze.md));
> `acetone migrate` rewrites history across a future format change.

## Use it

As a command-line workbench:

```sh
acetone init
acetone declare-label Host --key name
acetone query 'CREATE (:Host {name: "web1", os: "linux"})'
acetone commit -m "seed"
acetone query 'MATCH (h:Host) WHERE h.os = "linux" RETURN h.name'
acetone log
```

As a library — depend on the [`acetone-core`](crates/acetone-core) crate, the
product surface:

```rust
use acetone_core::{InitOptions, Repository};

let repo = Repository::init("graph.git".as_ref(), InitOptions::default())?;
```

## Design record

The `docs/` directory is authoritative — read the spec before relying on
behaviour:

- [`docs/acetone-01-design-space.md`](docs/acetone-01-design-space.md) — vision,
  prior art, and the shaping decisions.
- [`docs/acetone-02-spec.md`](docs/acetone-02-spec.md) — the v0.1 specification
  (data model, storage, encodings, query language, diff/merge, CLI).
- [`docs/acetone-03-roadmap.md`](docs/acetone-03-roadmap.md) — the phased
  implementation plan.
- [`docs/adr/`](docs/adr) — architecture decision records.

## Architecture

A Cargo workspace of crates with strictly downward dependencies:
`acetone-cli` → `acetone-core` (facade) → `acetone-cypher` → `acetone-graph`
→ `acetone-model` → `acetone-prolly` → `acetone-store`.

## Building and releasing

```sh
cargo build --workspace
cargo test --workspace
```

Release artefacts and the crate-publish order are documented in
[`docs/RELEASING.md`](docs/RELEASING.md).

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.

[prolly]: https://docs.dolthub.com/architecture/storage-engine/prolly-tree
[opencypher]: https://opencypher.org/
