# Introduction

acetone is an embedded, single-node, **version-controlled labelled property
graph database**: Dolt-style prolly trees stored in a git-compatible object
store, queried with openCypher, operated as a workbench — a CLI and a Rust
library, not a server. You branch, diff, merge and time-travel a property graph
the way you branch, diff, merge and time-travel code.

This manual is written for **operators**: people running acetone against real
data, who need worked examples, end-to-end procedures, and a runbook for when
things go wrong.

## How the manual is organised

- **Part I — Getting started**: installing acetone, creating your first graph,
  and meeting the *asset registry* — the running example every later chapter
  builds on.
- **Part II — Working with a graph**: a Cypher query cookbook, importing data
  end to end, history and branching and merging, schema and indexes, and
  routine maintenance and migration.
- **Part III — When things go wrong**: the recovery runbook.
- **Part IV — Reference**: the library API and the CLI, command by command.
- **Part V — Appendices**: the openCypher conformance statement and a glossary.

## Relationship to the design record

The manual explains *how to operate* acetone. The authoritative statement of
*what acetone is* — data model, storage format, encodings, query semantics —
is the design record in the repository, which this manual links to rather than
duplicates:

- [`docs/acetone-01-design-space.md`](https://github.com/curvelogic/acetone/blob/main/docs/acetone-01-design-space.md)
  — vision, prior art, and the shaping decisions.
- [`docs/acetone-02-spec.md`](https://github.com/curvelogic/acetone/blob/main/docs/acetone-02-spec.md)
  — the specification.
- [`docs/acetone-03-roadmap.md`](https://github.com/curvelogic/acetone/blob/main/docs/acetone-03-roadmap.md)
  — the phased implementation plan.
- [`docs/adr/`](https://github.com/curvelogic/acetone/tree/main/docs/adr)
  — architecture decision records.

Where the manual and the design record disagree, the design record wins —
and the disagreement is a bug worth reporting.
