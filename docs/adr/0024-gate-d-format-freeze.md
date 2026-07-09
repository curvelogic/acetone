# ADR-0024: Gate D — freeze the on-disk format at `format_version = 1`

*Status: proposed (mid-phase decision gate; flagged for Greg's retrospective review at the Phase 6 boundary) · Date: 2026-07-09 · Bead: acetone-cbl.1*

## Context

Roadmap Gate D is the format freeze: before the 0.1 release we commit to the
on-disk format so that two builds claiming `format_version = 1` produce
byte-compatible repositories, and any later change is a deliberate version bump
carried by `acetone migrate`. Per CLAUDE.md, Gate D is a *mid-phase* gate —
decided by ADR so work proceeds, and flagged prominently for Greg's
retrospective review rather than blocking on his sign-off.

The freeze surface is the whole persistent encoding: the memcomparable key
tuple encoding (`acetone-model/keys.rs`, `graph_keys.rs`, `records.rs`), the
canonical CBOR value encoding (`values.rs`, `cbor.rs`), the schema encoding
(`schema.rs`), the manifest schema (`manifest.rs`), and the prolly node framing
plus content-defined chunker (`acetone-prolly/node.rs`, `chunker.rs`). The
manifest already stores `[format_version, body]` with the version read *first*
and unknown versions rejected (`manifest.rs:190`), and `FORMAT_VERSION = 1`.

Three fresh, adversarial format-freeze audits (one per encoding surface, strongest
model tier per ADR-0009 — the cost of a latent freeze defect is high) reviewed
the surface independently. All three returned **FREEZE-READY-WITH-NITS**: the
encodings are canonical (exactly one byte form per logical value), strict and
total on hostile input, order-preserving where required, and — with one
exception — golden-anchored. History independence and order-preservation are
robustly property-tested.

## Decision

**Freeze `format_version = 1` as the 0.1 on-disk format, unchanged in bytes.**
No encoding needed a byte-level change to be freeze-ready. The pre-freeze work
was test and documentation hardening, done in this bead:

1. **Golden-anchor the prolly format (the one real gap).** The audit's headline
   finding: every other artefact had an absolute byte-pin, but the prolly node
   framing and chunker output were guarded only by *self-relative* property
   tests (`apply_batch == bulk_load`, revert-restores, cross-store equality),
   which pass equally on an internally-consistent format *drift* — silently
   changing every root hash under an unchanged `format_version`. Closed by
   `acetone-prolly/tests/golden.rs`: exact bytes of a leaf node and an inner
   node, plus the root hash of each, under the default chunk parameters.

2. **Correct spec §3.4.** The normative text said keys use "length-framed
   UTF-8" — a literal length-prefix would *break* order-preservation (the
   classic memcomparable trap). The implementation correctly uses
   order-preserving *chunked* framing; the spec now says so, removing a
   misdirection a future maintainer could "fix" the code toward.

3. **Ratify the deliberate scalar-domain caps** in spec §3.4, so a user does
   not discover them post-freeze: integers `i64`; `DateTime` is `i64`
   nanoseconds → representable instants ≈ **1677-09-21 … 2262-04-11**; UTC
   offset stored in whole minutes (`i16`, ±18:00), truncating sub-minute
   historical offsets; `Duration` unnormalised `(i64,i64,i64)`; floats `f64`.
   Widening any of these is a future bump handled by `migrate`.

### Flagged decision: the schema index entry stays `{label, property}`

The schema index definition is a fixed single-`(label, property)` equality
index; it has no composite (multi-property), kind, sort-direction, uniqueness,
or partial-predicate fields. Widening it is *free now* (no data in the wild)
and a *history-rewriting migrate* later. **We freeze it as-is.** Rationale: no
scheduled index work needs a wider *definition* — the open index beads
(`acetone-ryg` uniqueness, `acetone-6g5.3.3` IndexRange, `acetone-cbl.11`
store-backed seek) are all about index *usage*, not composite definitions — and
speculatively widening the frozen format to a half-designed shape risks
freezing a *worse* format than a clean single-property one that `migrate` can
evolve when composite indexes are actually designed. Labels already carry
composite keys, so identity is unaffected. **This is the one Gate D choice a
reviewer might make differently; it is called out here and in the phase report
for Greg's retrospective review.**

## Consequences

- `format_version` remains `1`; the version is checked first on every manifest
  decode, so a future-version repository is rejected with a clear error, never
  misread. The golden pins (values, keys, records, schema, manifest, **and now
  prolly**) fail loudly on any byte drift.
- The freeze does **not** by itself provide the version-bump machinery or
  `acetone migrate`; those are the escape hatch that makes the freeze safe and
  are built next (`acetone-cbl.1a`), demonstrated across a format bump.
- **Post-freeze, already tracked / deferred (not freeze-blocking):**
  - Array-preallocation amplification in the CBOR/value decoder (a robustness
    /DoS concern, *not* an on-disk-bytes issue — fixable with no version bump):
    `acetone-8gp`.
  - Cheap test-gap additions the key audit noted (CBOR text-head length
    boundary at 255/256 bytes; shared-prefix relationship-type ordering) —
    logic is sound by construction; tests are nice-to-have.
  - No explicit commit-time guard that a commit's `chunk_params` equal its
    parent's (they propagate from the manifest and every version is read
    self-consistently, so this is a belt-and-braces check, not a defect).
- The manifest is not self-describing about object format (SHA-1 vs SHA-256);
  hash width is validated-not-assumed on every read and the git repo config is
  the source of truth. Left as-is (documented here).
