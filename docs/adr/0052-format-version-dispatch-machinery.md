# ADR-0052: Ship the format_version dispatch machinery; prove it with a synthetic v2, defer a real v2

*Status: accepted — pending ratification at the Phase 8 / 0.3 boundary · Date: 2026-07-22 · Bead: acetone-5yr*

## Context

ADR-0048 chose **read-old-write-new** as acetone's default format-evolution
path: the binary retains a decoder for every format version it has ever shipped,
dispatches on the manifest's `format_version`, and never rewrites history to
cross a format boundary. `acetone-5yr` is the bead that turns that policy into
code, and it is Phase 8 **exit criterion 3**: *"a `format_version` bump applied
to a live graph via read-old-write-new — no history rewrite, no force-push."*

The manifest was built for this. Its top level is the stable two-element array
`[format_version, body]`; a reader reads the version *first* and only then
interprets `body` as version-`N` territory (`manifest.rs`). The single thing
stopping a repository holding a mix of versions is one line of policy:
`Manifest::decode` rejects any `version != FORMAT_VERSION` with
`UnsupportedVersion` instead of dispatching to that version's decoder. That
rejection is the deferred half of Gate D's "`format_version` bump machinery"
(ADR-0024).

Implementing the dispatch is unambiguous. But *demonstrating* a live v1→v2 bump
needs a v2 to bump to — and Phase 8 introduced no change that forces a real
format bump. The four co-tenancy ref assumptions (ADR-0049/0050/0051) flipped
entirely at the ref/store layer; none of them altered the on-disk *encoding*.
So exit criterion 3 confronts the same fork ADR-0025 faced when it built the
`migrate` engine before there was any format to migrate: **build and prove the
mechanism now, or invent a format change purely to exercise it?**

Two options:

- **(A) Ship a real `format_version = 2`** — make a minimal deliberate body
  change (e.g. add a field), retain the v1 decoder, add a v2 golden. This meets
  exit criterion 3 to the letter. But it re-pins *every* manifest and commit
  golden (all those hashes change), is heavy Gate-D churn, and — the decisive
  objection — **permanently changes acetone's on-disk format purely to have
  something to demonstrate.** ADR-0025 explicitly warned against inventing a
  synthetic format just to demo a mechanism.

- **(B) Ship the dispatch machinery, keep `FORMAT_VERSION = 1`** (no golden
  churn, shipped format byte-for-byte unchanged), and prove cross-version
  coexistence with a test that registers a *synthetic* v2 decoder and
  hand-crafts a content-addressed store holding both a v1 manifest and a v2
  manifest — asserting both decode, the v1 object's hash is unchanged by the v2
  write, and a production (v1-only) build rejects v2 as an unknown future
  version. A real shipped v2 is deferred to the first genuine format change,
  which will land through exactly this machinery.

## Decision

**Adopt (B): ship the `format_version` dispatch machinery now; prove it with a
synthetic v2 in tests; defer a real shipped `format_version = 2` to the first
format change that genuinely warrants one.**

`Manifest::decode` becomes a dispatch over a table of retained per-version
decoders (`DECODERS`), which today holds exactly one row — `(1, decode_v1_body)`.
The outer-envelope read (`[version, body]`) and the version→decoder lookup are
factored into a shared `decode_with(bytes, decoders)` seam; production decode
calls it with `DECODERS`, and the coexistence test calls it with a two-row table
`(1, decode_v1_body) + (2, decode_v2_synthetic)`. The v1 body reader is factored
into `read_body_current` so the shipped v1 path is byte-identical to before
(regression-guarded) and the synthetic v2 decoder can reuse it. Writes are
untouched: `encode` always emits `FORMAT_VERSION`.

This is the same "engine now, real demonstration deferred" call Greg accepted
for `migrate` at the 0.2 boundary (ADR-0025): the mechanism is built, exercised,
and reviewed; the first real use lands when the product actually needs it,
through the seam that is now in place.

## Consequences

- **Exit criterion 3 is met in the "machinery shipped + cross-version
  coexistence proven" sense, not the "a real v2 ships in 0.3" sense.** This is a
  deliberate deviation from the letter of ADR-0048's consequence note (which
  anticipated a real v2 in Phase 8) and is **flagged for Greg's ruling at the
  boundary**: accept the machinery-proven interpretation (recommended), or
  require a real shipped v2 before the gate closes.
- **Zero format change, zero golden churn.** The shipped `format_version` stays
  `1`; no manifest/commit hash changes; no golden re-pin. The refactor is
  behaviour-preserving for every existing repository, which the existing
  round-trip and golden tests continue to guard.
- **The dispatch seam is the retained-reader machinery ADR-0048 requires.** When
  a real v2 lands, it is one new `DECODERS` row plus its golden; the v1 decoder
  already sits behind the seam as a distinct, independently-pinned reader.
- **Unknown/future versions still fail loudly.** A version not in `DECODERS` (a
  repository written by a *newer* build) is rejected with `UnsupportedVersion`,
  exactly as Gate D requires — the machinery is opt-in per retained reader, not
  a licence to guess at formats it has never seen.
- **The synthetic v2 decoder lives only in tests.** It ships in no binary; it
  exists to prove the dispatch routes by version and that a v1 object is
  untouched when a v2 object joins it in the same store.
- **Revisit at:** the first genuine format change (which becomes the real v2,
  landing through this seam with a v2 golden), or if Greg rules at the boundary
  that a real v2 must ship in 0.3.
