# ADR-0008: Map layouts and manifest encoding

Status: accepted (agent decision, flagged for phase-boundary review)
Date: 2026-07-04
Bead: acetone-63m.4

## Context

Spec §3.3 names the v0.1 maps (`schema`, `nodes`, `edges_fwd`, `edges_rev`,
`idx/<name>`, `conflicts`) and the manifest, but leaves the byte-level
layouts open. These are format decisions: every choice here is normative
once made, pinned by golden vectors, and changeable only with a
`format_version` bump (spec §10). ADR-0004 fixed the two primitive
encodings (memcomparable keys, canonical CBOR values); this ADR fixes how
those primitives compose into map keys, records and the manifest.

## Decision

**One encoding for node identity, everywhere.** A node key is encoded as a
single memcomparable `List` element — `List([String(primary_label),
key_0 … key_n])` — and those exact bytes are used standalone as the
`nodes`-map key and embedded inside edge and index keys. Rationale: the
node key is the node's identity (Load-Bearing Invariant 3); giving it one
canonical byte form means identity comparisons are byte comparisons in
every map, and nothing can drift. The list wrapper makes the key
self-delimiting inside composite keys (variable-arity key tuples would
otherwise be ambiguous), and the list terminator `0x00` sorts below every
type tag, so label-prefix and whole-key-prefix range scans work
unchanged. Cost: two framing bytes per nodes-map key.

**Edge keys.** `edges_fwd` key = `[node_key(src), String(type),
node_key(dst), disc]`; `edges_rev` key = `[node_key(dst), String(type),
node_key(src), disc]` with an empty value (spec §3.3). The discriminator
is a scalar `Value`, encoded as `Null` when defaulted — `Null` has the
lowest type tag, so default-discriminator edges sort first within a
`(src, type, dst)` group. "All edges out of node X" and "all `T`-edges
out of X" are both prefix scans.

**Index keys.** `idx/<name>` key = `[String(label), String(property),
value, node_key]`, empty value — exactly the spec §3.3 tuple, even though
label and property are constant within one index map today. The
redundancy keeps the door open to shared or composite index maps without
a key-layout change. NaN float values are unencodable in key position
(ADR-0004); the graph layer must skip or reject such entries when it
maintains indexes (recorded consequence, decision deferred to the
index-maintenance bead).

**Schema map.** Key = `[String(kind), String(name)]`, `kind` ∈
`"label" | "rtype" | "index"`; one prefix scan per kind. Values are
canonical CBOR **text-keyed maps** (field names as keys, sorted per
RFC 8949 §4.2.1).

**Node and edge records.** `nodes` value = CBOR array
`[secondary_labels, properties]` with secondary labels sorted and
deduplicated; `edges_fwd` value = the properties map directly. These are
**positional arrays / bare maps**, not field-named maps: records are the
hot path and dominate repository size; schema and manifest are cold and
tiny, so they get self-describing field names instead. Pre-1.0 there is
no compatibility cost either way (`format_version` + `acetone migrate`
cover evolution).

**Key properties are excluded from node records.** The record carries
secondary labels and non-key properties only; key values live solely in
the map key. A record that disagrees with its key is therefore
unrepresentable, which is the strongest possible enforcement of
Invariant 3 (`SET` must never modify key properties) at the storage
layer. Consequence: reconstructing a node's full property map requires
the schema's key declaration (to name the key columns); the graph layer,
which always holds the schema, does the recombination.

**Manifest.** The canonical CBOR two-element array
`[format_version, body]`. Putting the version in a fixed leading
position (rather than as a body field, where canonical map ordering
would sort it *last*) means any reader — including one from a different
format era — reads the version first and can stop; the outer shape is
documented as stable across future bumps so a newer repository is always
identifiable. `body` is a canonical text-keyed map: `chunk_params`
(`[min_bytes, mask_bits, max_bytes]`, stored once — fixed at init per
spec §3.2), `schema`/`nodes`/`edges_fwd`/`edges_rev` (`[hash bytes,
height]`), `indexes` (map: index name → `[hash bytes, height]`),
`conflicts` (same shape, **absent** unless a merge is in progress).
Chunk hashes are embedded as byte strings via `Hash::as_bytes` (opaque,
width set by the repository's object format). `FORMAT_VERSION = 1`; any
other version is rejected with a dedicated error. Manifest encoding is a
pure function of the struct — determinism is property-tested and golden-
pinned, satisfying "manifest hashing deterministic".

## Consequences

- All five layouts join the golden-vector suite; any byte change is a
  `format_version` bump.
- The low-level CBOR reader/writer moves from `values.rs` into a
  crate-internal `cbor` module so record/schema/manifest encoders reuse
  it (records need CBOR maps, which the value layer deliberately
  rejects). `values.rs` behaviour is unchanged and its tests guard the
  refactor.
- The graph layer inherits three obligations: enforce key-property
  exclusion when building records, recombine key properties on read, and
  decide NaN handling for indexes.
- `conflicts` map layout (spec §6 structured records) is deliberately
  **not** fixed here — it belongs to the Phase 4 merge beads; the
  manifest just reserves its slot.
