# ADR-0060: Autodeclare is strictly opt-in — rel-types first, node labels gated on key inference

*Status: accepted — direction proposed by Greg while dogfooding 0.1.0 (2026-07-11), analysis recorded on the bead, ratified by Greg at the Phase 9 grooming session (2026-07-24) · Date: 2026-07-24 · Bead: acetone-auz*

## Context

Acetone's schema is mandatory for identity: every node label must declare a
key before Cypher can persist nodes of it, and every relationship type must
be declared before use (spec §2). That is exactly right for the
audited-registry use case — schema changes are deliberate history — and
exactly wrong for interactive experimentation, where the declare-first
round-trip is the single largest piece of dogfooding friction.

The two halves of the problem are not symmetric:

- A **relationship type** is just a name — no key, no shape. Declaring it on
  first use is a deterministic schema append with no identity commitment.
- A **node label** carries identity: Invariant #3 makes a node's identity
  `(primary label, key tuple)`, so autodeclaring a label means **guessing
  its key** — a load-bearing choice affecting identity, uniqueness and
  merge. A wrong guess silently mis-models identity, which is *worse* than
  today's honest error.

Two tensions to respect: implicit schema mutations folded into data writes
cut against schema-as-deliberate-history (fine for scratch, wrong for a
registry); and two branches autodeclaring the same label with different
inferred keys create a new schema-conflict class, so inference must be
deterministic and divergence must surface through the ordinary merge
conflict machinery, not silently pick a winner.

## Decision

Autodeclare ships **strictly opt-in, off by default** — a per-invocation
flag / shell mode or a per-repository "scratch" setting; the default
experience keeps declaration deliberate everywhere.

- **Relationship-type autodeclare first**: on first use in a write, the
  type is appended to the schema deterministically. Low risk, high value.
- **Node-label autodeclare is gated on a deterministic key-inference
  rule**: a single-property node infers that property as the key; else a
  conventional key name (`id`, `name`) if present; else the write still
  errors demanding an explicit declaration. **Never a surrogate auto-id** —
  natural keys stay mandatory, or the merge and history story degrades.
- Divergent autodeclared keys across branches are a **schema conflict**,
  surfaced by merge like any other conflict. Node-label autodeclare does
  not ship before that conflict story exists.

This is a UX layer over declaration, not a weakening of Invariant #3: an
autodeclared label always ends up with a real, deterministically inferred
key, and identity is never invented silently.

## Consequences

- Registry-grade guarantees are untouched by default; experimentation gets
  a sanctioned fast path instead of ad-hoc workarounds.
- Implementation is deliberately **unscheduled** (backlog bead
  acetone-nc91): Phase 9 is scale and conformance; this waits for a phase
  where dogfood UX ranks.
- The rel-type half can ship alone; the node-label half is blocked on the
  key-inference rule *and* the schema-conflict surfacing, in that order.
- The decision bead acetone-auz closes against this ADR.
