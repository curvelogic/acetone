# acetone user guide

acetone is a version-controlled property-graph workbench: you `init` a
repository, evolve a graph with openCypher and plumbing commands, `commit`
snapshots, and `branch`/`merge`/time-travel the history exactly as you would
with code. This guide walks the everyday workflow. The authoritative reference
is [the spec](acetone-02-spec.md); the query subset is described in
[the conformance statement](conformance.md).

## Install

Build from source:

```sh
cargo build --release        # target/release/acetone
```

or download a release binary (a self-contained static binary on Linux — see
[RELEASING.md](RELEASING.md)). All commands take `--repo <path>` (default: the
current directory).

## Create a repository

```sh
acetone init                 # a bare repo in the current directory
acetone init --object-format sha256   # SHA-256 object format (default: SHA-1)
```

An acetone repository *is* a git repository: its history is a git commit graph,
and `git log`, `git push`, `git clone` all work on it.

## Declare a schema

Node identity is `(primary label, key tuple)` — natural keys are mandatory and
declared before Cypher can persist nodes of a label (Invariant #3):

```sh
acetone declare-label Host --key name
acetone declare-label Service --key name --require owner   # existence constraint
acetone declare-rel-type DEPENDS_ON
acetone declare-index host_by_os --label Host --property os   # equality index
```

## Query and mutate with Cypher

```sh
acetone query 'CREATE (:Host {name: "web1", os: "linux"})'
acetone query 'MATCH (h:Host {os: "linux"}) RETURN h.name ORDER BY h.name'
acetone query 'MATCH (h:Host {name:"web1"}), (s:Service {name:"db"})
               MERGE (h)-[:DEPENDS_ON]->(s)'
acetone query 'MATCH (h:Host) WHERE h.os = "bsd" SET h.os = "freebsd"'
```

`--at <refspec>` runs a read against any point in history (time travel);
`--format table|json|csv` picks the output shape. `acetone shell` is a readline
Cypher REPL with `:checkout`, `:log`, `:diff` conveniences.

Plumbing commands (`put-node`, `put-edge`, `get-node`, `list-nodes`, `rekey`)
manipulate single entities without Cypher — handy in scripts.

## Commit, branch, merge, time-travel

```sh
acetone commit -m "seed infrastructure"
acetone log
acetone branch feature           # create a branch at HEAD
acetone checkout feature
# … make changes, commit …
acetone checkout main
acetone merge feature -m "merge feature"   # three-way merge (spec §6)
acetone diff main feature         # structural diff between two versions
```

A clean three-way merge commits directly. A conflict puts the workspace into a
merging state with a queryable `conflicts` map; resolve with ordinary writes or
`acetone resolve --all-ours|--all-theirs`, then `acetone commit`.

## Import and export

```sh
acetone import --format csv hosts.csv --label Host          # a row per node
acetone import --format csv links.csv --edge DEPENDS_ON --from src --to dst
acetone export --format csv --label Host --out hosts.csv    # round-trips to identical roots
acetone export --format json --out export/  # no --label/--edge: a directory, one file per label/type
```

Import records provenance trailers (`Acetone-Source`, `-Extractor`,
`-Source-Hash`) and detects a no-op when the source is unchanged — scheduled
re-imports become clean diffs.

## Maintain

```sh
acetone reindex     # rebuild declared indexes from nodes (a no-op when consistent)
acetone fsck        # verify chunk reachability, manifest integrity, edge symmetry,
                    # index consistency and history-independence spot-checks
acetone gc          # consolidate the object store into a packfile, reclaim space
```

## Migrate across a format change

The on-disk format is frozen at `format_version = 1`
([ADR-0024](adr/0024-gate-d-format-freeze.md)). When a future release bumps the
format, `acetone migrate` rewrites all history forward (new hashes, preserving
messages, authorship and timestamps). Today it also re-chunks a repository under
new chunk parameters:

```sh
acetone migrate --min-bytes 1024 --mask-bits 12 --max-bytes 65536
```

Migration requires a clean, non-merging workspace and resets it to the rewritten
head. See [ADR-0025](adr/0025-history-rewrite-migrate-engine.md).

## Use it as a library

Depend on the [`acetone-core`](../crates/acetone-core) crate — the product
surface — and drive a [`Repository`](../crates/acetone-graph/src/repo.rs)
directly:

```rust
use acetone_core::{InitOptions, Repository};

let repo = Repository::init("graph.git".as_ref(), InitOptions::default())?;
# Ok::<(), acetone_core::GraphError>(())
```
