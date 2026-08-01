# API stability

From **0.2**, the `acetone-core` library exposes a **frozen public API**
(ADR-0046). This document is the contract: what is guaranteed, what is not, and
how the guarantee is enforced.

## What is guaranteed

The **curated headline surface** — the items re-exported flat at the
`acetone-core` crate root — is stable and follows semantic versioning:

- Repository & history: `Repository`, `Transaction`, `Snapshot`, `InitOptions`,
  `LogEntry`, `DEFAULT_BRANCH`, `DEFAULT_WORKSPACE`, `GraphError`
  (`#[non_exhaustive]` from 0.4 — match it with a wildcard arm, and new
  variants will never break your build).
- Migrate: `FormatTransform`, `Rechunk`, `MigrateReport`, `rewrite_history`.
- Query: `Session`, `Outcome`, `QueryError`, `QueryLimits`, `QueryResult`,
  `ResourceLimit`, and `QueryValue` — the value type of query result rows and
  `run_with` parameters (distinct from the stored-domain `Value`).
- Values, keys & records: `Value`, `NodeKey`, `EdgeKey`, `NodeRecord`,
  `EdgeRecord`.
- Store: `Hash`, `ObjectFormat`.

**Semver policy:** within a **patch series** (0.2.x, 0.3.x, …), changes to
this surface are **additive only** (new items, new methods) — no removals,
renames, or signature changes. A breaking change requires bumping the
**minor** version (0.2.x → 0.3.0 → 0.4.0, …). Pre-1.0, a bumped minor is the
*permission* for breakage, not a promise of it — 0.3.0, for instance, changed
nothing in this surface.

## What is NOT guaranteed

- **Deep access.** `acetone-core` also re-exports the constituent crates as
  modules (`acetone_core::cypher`, `::graph`, `::model`, `::store`) for full
  access. Items reachable **only** through these modules — anything not in the
  curated list above — may change in any release. Depend on the crate-root
  re-exports for stability; reach into the modules only when you accept churn.
- **The CLI.** `acetone`'s command surface and output formats (including
  `--json`) are a **separate** product surface (spec §7) and are not covered by
  this document. The `--json` shape is explicitly **unstable pre-1.0**: it may
  change at any minor release, with the change noted in the CHANGELOG. Pin
  your acetone version if you script against exact field names or nesting.
- **The on-disk format.** That is frozen separately at `format_version 1`
  (Gate D, ADR-0024) and guarded by the prolly/model golden pins.

## How it is enforced

Five committed snapshots, checked by the CI `public-api` job (ADR-0046) — the
API analogue of the format goldens:

- `crates/acetone-core/public-api.txt` — the curated re-export **list**, so a
  symbol added to or removed from the frozen surface is caught.
- `crates/acetone-cypher/public-api.txt`, `crates/acetone-graph/public-api.txt`,
  `crates/acetone-model/public-api.txt`, `crates/acetone-store/public-api.txt` —
  the **full-signature** surfaces of the crates hosting every frozen type,
  recorded field by field, variant by variant, method signature by method
  signature, attributes included.

Every type on the frozen surface is therefore **signature-tracked in the crate
that hosts it**: a change *inside* a re-exported type — a new field or variant,
a removed or re-signatured inherent method, an added `#[non_exhaustive]` — is a
snapshot diff, wherever the type lives. (This closed `acetone-7qw.20`/`.5` in
Phase 10: previously only `acetone-cypher` was signature-tracked, and making
`GraphError` — hosted by `acetone-graph` — `#[non_exhaustive]` in 0.4 was
source-breaking yet produced an empty snapshot diff.)

Note the graph/model/store snapshots record those crates' **entire** public
surfaces, which is wider than the frozen contract: the deep-access surface
(paths through the `graph`/`model`/`store` modules) remains explicitly
unfrozen, so a diff in those snapshots is not automatically a contract break —
it is a prompt for deliberate review and, where the curated surface is
touched, a CHANGELOG entry. What is *promised* is still defined by this
document; the snapshots define what is *seen*.

Any drift fails CI. After an **intentional** change, re-bless and commit the
snapshots:

```sh
scripts/bless-public-api.sh   # or the per-package command the CI error prints
```

**Tooling pin.** `cargo-public-api` reads rustdoc's *unstable* JSON, so the CI
job pins **both** a nightly toolchain and a `cargo-public-api` version known to
parse it (at introduction: `nightly-2026-07-18`, rustdoc JSON `format_version`
60, `cargo-public-api` 0.52.0). Bump the nightly, the tool, and the snapshots
together.
