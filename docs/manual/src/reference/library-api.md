# The library API

acetone is usable as an embedded Rust library, and the library is a first-class
product surface. The single dependency a consumer adds is **`acetone-core`**: a
façade crate that re-exports everything the constituent crates provide, with a
small, curated, *stable* API at its root. The `acetone` CLI is a thin client
over exactly the same crates, so anything the CLI does, your program can do.

## The stability contract

From 0.2, the curated headline surface — the types and functions re-exported
flat at the `acetone-core` crate root — is **frozen** (ADR-0046) and follows
semantic versioning: additive-only within a patch series (0.2.x, 0.3.x, …),
with a breaking change requiring a minor bump. The freeze is enforced
mechanically, not just promised: committed `cargo-public-api` snapshots are
checked by CI, so the surface cannot drift silently — the API analogue of the
on-disk format's golden pins.

The authoritative statement of what is and is not guaranteed is
[`STABILITY.md`](https://github.com/curvelogic/acetone/blob/main/STABILITY.md)
in the repository root. In outline, the frozen surface is:

- **Repository & history** — `Repository`, `Transaction`, `Snapshot`,
  `InitOptions`, `LogEntry`, `DEFAULT_BRANCH`, `DEFAULT_WORKSPACE`,
  `GraphError`.
- **Query** — `Session`, `Outcome`, `QueryError`, `QueryLimits`,
  `QueryResult`, `ResourceLimit`, and `QueryValue` (the value type of query
  result rows and `run_with` parameters).
- **Values, keys & records** — `Value` (the stored domain), `NodeKey`,
  `EdgeKey`, `NodeRecord`, `EdgeRecord`.
- **Migrate** — `FormatTransform`, `Rechunk`, `MigrateReport`,
  `rewrite_history`.
- **Store** — `Hash`, `ObjectFormat`.

### Not on crates.io (yet)

acetone is deliberately **not published to crates.io** until the project is
judged mature enough, or an external need forces it (ADR-0047). Depend on it
as a git or path dependency:

```toml
[dependencies]
acetone-core = { git = "https://github.com/curvelogic/acetone" }
```

Because there is no crates.io release there is also no docs.rs; build the API
documentation locally with `cargo doc -p acetone-core --open`. A docs.rs link
will replace this chapter's listing once the publication decision changes.

## A minimal end-to-end example

The following program initialises a repository, declares a label's natural
key, writes through Cypher, commits, and reads the data back. It compiles and
runs against acetone 0.3.0 exactly as shown (the commit hash embeds a
timestamp, so yours will differ; the query row reproduces exactly). It is
compiled, run and output-checked in CI as the cargo example
[`manual_library_api`](https://github.com/curvelogic/acetone/blob/main/crates/acetone-core/examples/manual_library_api.rs)
— the listing below *is* that file, included verbatim, so the manual cannot
drift from code that works (`cargo run -p acetone-core --example
manual_library_api` runs it from a checkout):

```rust
{{#include ../../../../crates/acetone-core/examples/manual_library_api.rs}}
```

Output:

```text
committed 5d8380e1be71f57b724d6dda0a59e64f4aa5408e
[String("web1"), Int(8)]
```

The pieces, in the order they appear:

- **`Repository::init` / `Repository::open`** create or open a repository;
  `InitOptions` selects the object format (SHA-1 by default) and the
  co-tenant layout. The `Repository` value is the root of everything else:
  branching (`create_branch`, `checkout_branch`), history (`log`, `diff`,
  `snapshot`), merging (`merge`, `resolve_all`, `abort_merge`) and
  maintenance (`reindex`, `gc`).
- **`Repository::begin_write`** starts the single-writer **`Transaction`**.
  Direct mutations (`put_node`, `put_edge`, `put_schema`, `delete_node`, …)
  accumulate in the transaction; `save()` atomically advances the workspace,
  while `commit(message, trailers, author)` turns the workspace's state into
  a git commit and returns its `Hash`.
- **`Session`** is the governed query entry point (ADR-0039). `run` executes
  a read or write query against the workspace with default limits; `run_with`
  takes explicit parameters and a `QueryLimits` governor budget; `query_at`
  runs a read-only query against any historical version — time travel by
  refspec. A write query advances the workspace exactly as `save()` does; it
  still takes a separate `commit` to make history.
- **`Outcome`** distinguishes `Read` from `Write`; either way
  `outcome.result()` yields the **`QueryResult`** — `columns`, `rows`, write
  `stats` and non-error `advisories`. Row elements are **`QueryValue`**, the
  runtime value carrier of the query interface — distinct from the
  stored-domain `Value` used in records and keys.

## Deep access — the unstable escape hatch

`acetone-core` also re-exports the constituent crates whole, as modules:
`acetone_core::cypher`, `::graph`, `::model` and `::store`. Everything those
crates make public is reachable — but anything reachable *only* through these
modules is **not** covered by the stability guarantee and may change in any
release, including a patch release.

The example above uses the seam honestly: declaring schema currently requires
`acetone_core::model::schema::{LabelDef, SchemaEntry}` from the deep-access
surface (the CLI's `declare-label` does exactly the same). Reach into the
modules when you need to, but treat every such import as a marker of code
that may need attention on upgrade — and depend on the flat crate-root
re-exports everywhere you can.
