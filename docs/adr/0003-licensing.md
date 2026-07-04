# ADR-0003: Licensing — MIT OR Apache-2.0; scoped MPL-2.0 exception for uluru

*Status: accepted (Greg's ruling, 2026-07-04, Phase 0 boundary) · Beads:
acetone-63m.1 (gated on the exception)*

## Context

Two licence questions were queued for Greg at the Phase 0 boundary: the
product licence (crates carried `publish = false` and no `license` field),
and gitoxide's MPL-2.0 transitive dependency (`uluru`, an LRU cache pulled
in via gix-pack), which the deny.toml allowlist rejected.

## Decision

Greg ruled: **acetone is dual-licensed `MIT OR Apache-2.0`** (the Rust
ecosystem convention), and **`uluru` gets a crate-specific exception** in
deny.toml rather than a general MPL-2.0 allowance — MPL remains denied in
general, so any future MPL dependency triggers a fresh, deliberate decision.

## Consequences

`license.workspace = true` across all crates; `LICENSE-MIT` and
`LICENSE-APACHE` at the repo root; deny.toml's `private.ignore` flipped to
false so CI licence checking genuinely covers our own crates (it previously
skipped them as unpublished). MPL-2.0's file-level copyleft on uluru is compatible with
permissive distribution (its files must stay MPL if modified; we do not
modify them). Crates remain `publish = false` until 0.1 packaging.
