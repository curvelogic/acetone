# ADR-0026: `acetone-core` façade crate and release packaging

*Status: accepted — ratified by Greg at the pre-0.1 boundary review (2026-07-11) · Date: 2026-07-09 · Beads: acetone-cbl.7, acetone-cbl.5*

## Context

Spec §7 calls the library API `acetone-core` — "the real product surface" — but
§8's crate list had no such crate; the scaffold followed §8 (`acetone-store`,
`-prolly`, `-model`, `-graph`, `-cypher`, `-cli`). A library consumer therefore
had to depend on several internal crates and know which types live where. PR #2
review flagged the inconsistency (acetone-cbl.7), to be resolved deliberately
per CLAUDE.md's never-silently rule: introduce `acetone-core`, or amend §7.

Packaging for 0.1 (acetone-cbl.5) needs the same thing: "acetone-core published
as a library crate (the real product surface)" plus a single static binary.

## Decision

**Introduce `acetone-core` as a façade crate**, and update spec §8 to list it —
making §8 match §7 rather than watering §7 down.

- `acetone-core` re-exports the constituent crates as modules (`graph`,
  `model`, `cypher`, `store`) for full access, and re-exports the headline
  types (`Repository`, `InitOptions`, `Transaction`, `Snapshot`, `Value`,
  `NodeKey`/`EdgeKey`, `NodeRecord`/`EdgeRecord`, `migrate`, `fsck`, …) flat at
  the crate root. It is the single dependency a library consumer adds; the CLI
  stays a thin client over the same surface. A façade (not a code move) keeps
  the strictly-downward crate layout and the existing test/ownership boundaries
  intact.

### Packaging

- **Publish metadata.** Every library crate carries `license`, `description`,
  `repository` and a `version`; path dependencies now also carry a `version`,
  so `cargo publish` can resolve them. The six library crates
  (`store`/`prolly`/`model`/`cypher`/`graph`/`core`) are `publish = true`;
  `bench`/`tck`/`lab` stay `publish = false`.
- **Dry-run reality.** `acetone-core` is a façade, so its crates must be
  published **bottom-up** (crates.io requires each dependency to be on the index
  first). The leaf `acetone-store` dry-runs clean today; a full-chain dry-run is
  only possible once the lower crates are published, which is inherent to any
  unpublished workspace. Publishing for real is deferred to the 0.1 tag (it
  reserves names irreversibly) and documented in `docs/RELEASING.md`.
- **Single static binary.** A `[profile.release]` (`strip`, `lto`, one codegen
  unit) and a `Release` workflow build the `acetone` binary per target on a
  `v*` tag: statically-linked musl on Linux (`x86_64`, `aarch64`), the platform
  binary on macOS (fully static is unsupported there). Artefacts are a
  `.tar.gz` plus a `.sha256`.

## Consequences

- New crate `crates/acetone-core`; spec §8 lists it; `docs/RELEASING.md` and a
  root `README.md` document release and publish. No behavioural change to any
  existing crate — the façade only re-exports.
- The workspace is publish-ready; actually publishing to crates.io and tagging
  0.1 remain deliberate manual steps at the release boundary.
- Consumers should migrate from depending on internal crates to `acetone-core`;
  the internal crates remain public (the façade re-exports them), so this is a
  convenience, not a breaking constraint.
