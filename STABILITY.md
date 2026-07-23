# API stability

From **0.2**, the `acetone-core` library exposes a **frozen public API**
(ADR-0046). This document is the contract: what is guaranteed, what is not, and
how the guarantee is enforced.

## What is guaranteed

The **curated headline surface** — the items re-exported flat at the
`acetone-core` crate root — is stable and follows semantic versioning:

- Repository & history: `Repository`, `Transaction`, `Snapshot`, `InitOptions`,
  `LogEntry`, `DEFAULT_BRANCH`, `DEFAULT_WORKSPACE`, `GraphError`.
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
  this document.
- **The on-disk format.** That is frozen separately at `format_version 1`
  (Gate D, ADR-0024) and guarded by the prolly/model golden pins.

## How it is enforced

Two committed snapshots, checked by the CI `public-api` job (ADR-0046) — the API
analogue of the format goldens:

- `crates/acetone-core/public-api.txt` — the curated re-export **list**, so a
  symbol added to or removed from the frozen surface is caught.
- `crates/acetone-cypher/public-api.txt` — the **full-signature** surface of
  `acetone-cypher` (which hosts `Session`/`QueryLimits`/`QueryResult` and the
  runtime value carrier), so a signature-level change to the newest frozen query
  API is caught automatically.

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
