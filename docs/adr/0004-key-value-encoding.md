# ADR-0004: Key and value encoding format decisions

*Status: accepted · Date: 2026-07-04 · Bead: acetone-63m.3 · PR: #9*

## Context

Spec §3.4 makes the key and value encodings normative (Load-Bearing
Invariant 2): every root hash depends on them, and any change is a
`format_version` bump. Implementing them in `acetone-model` forced several
decisions the spec skeleton leaves open, one of which deliberately
supersedes the spec's current wording.

## Decision

1. **Strings/bytes in keys use chunked 8+1 framing, not length prefixes or
   NUL escaping.** Data is written in 8-byte groups, each followed by a
   marker byte (`0xff` = full group with more to come; `0xf7 + n` = final
   group with `n` meaningful bytes, zero-padded). This is order-preserving
   for arbitrary content (including embedded NULs), gives every byte
   string exactly one encoding, and costs a fixed 9/8 overhead. The spec's
   phrase "length-framed UTF-8" is superseded by this scheme (a plain
   length prefix does not preserve lexicographic order); the spec text
   should be aligned at its next revision rather than silently.
2. **NaN policy.** Keys: NaN is rejected at encode time — NaN's equality
   semantics poison identity and merge determinism. Values: NaN is
   permitted but every NaN encodes as the canonical quiet NaN `f9 7e00`
   (payloads are not preserved), so equal-looking values hash equally.
3. **Negative zero.** Keys: `-0.0` is normalised to `+0.0` (openCypher
   treats them as equal; two byte-distinct keys for one logical value
   would be a correctness bug). Values: the sign of zero is preserved.
4. **Cross-type key order is by type tag** — Null < false < true < Int <
   Float < String < Bytes < Date < Time < DateTime < Duration < List —
   stable and documented, not semantic. Int and Float do not interleave
   numerically. Consequences for the Cypher layer (numeric range scans
   must union the Int and Float tag ranges; `ORDER BY` over mixed-type
   values cannot be a raw index scan; NaN properties cannot yield index
   keys) are documented in the `keys` module.
5. **CBOR tags.** Date uses standard tag 100 (RFC 8943). Time, DateTime
   and Duration use acetone-assigned tags **74100–74102** from the
   first-come first-served range (≥ 32768, RFC 8949 §9.2), unregistered
   and format-internal. (Review corrected an initial assignment of
   4100–4102, which sits in the Specification Required range.)
6. **Values use RFC 8949 §4.2.1 core deterministic encoding**, including
   shortest-form floats (binary16/32/64 chosen by exact representability),
   with a hand-rolled writer/reader rather than a CBOR crate: strict
   canonical-form control on both encode and decode is the point, and the
   needed subset is small and exhaustively tested. Decoders are strict
   (only canonical bytes are accepted) and total (no panics, bounded
   allocation, depth-limited).
7. **Temporal representations** are plain integer forms (days since epoch;
   nanos since midnight, `< 86_400_000_000_000`; epoch-nanos + UTC offset
   in minutes, bounded ±1080; months/days/nanos for durations) — format
   control without calendar arithmetic or a chrono dependency.

## Consequences

The byte formats are pinned by golden vectors in
`crates/acetone-model/tests/golden.rs` (the format_version 1 fixtures) and
by round-trip/ordering/canonicity property tests. Changing any decision
above re-hashes every repository (acceptable pre-1.0, spec §10). The index
layer must still decide how to handle NaN in indexed float properties
(skip or reject — decision 2 forecloses storing them). Revisit triggers:
GQL alignment forcing nested maps into values, or Phase 2 planner work
showing the Int/Float tag split costs more than the two-range union it
was traded for.
