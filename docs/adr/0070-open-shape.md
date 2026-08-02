# ADR-0070: Declared property types do not close a label's shape

*Status: accepted — Phase 10 unit acetone-7qw.17; queued for Greg's boundary review · Date: 2026-08-02 · Bead: acetone-7qw.17*

## Context

Since `declare-label --type` shipped (PR #226), a binder rule that predates
it became reachable: a node-pattern map literal on a label with any declared
property type could only name declared or key properties. `CREATE (:Host
{hostname: 'a', ip: '…'})` failed with `unknown property "ip"` once `Host`
declared *any* type — while `MATCH … SET h.ip = '…'` was accepted and read
back, and `put-node --prop` likewise. The Phase 9 final correctness review
filed the asymmetry (acetone-7qw.17) with the question: should non-empty
`types()` imply a closed shape at all?

## Decision

**No. Open shape everywhere.** Declared types constrain exactly the
properties they name; property names outside the declaration are legal on
every path — `CREATE`/`MERGE`/`MATCH` map literals, `SET`, and plumbing —
identically. The binder's closure rule is removed (the `UnknownProperty`
bind error with it), and the asymmetry is resolved in the open direction.

Because on a label that *has* declared types an off-catalogue name is
likelier a typo than a deliberate extension, the protection the closure
accidentally provided survives as an **advisory** on the established
channel (acetone-7bn.5, acetone-2ck.3): Strict-mode binding collects
`(label, property, did-you-mean)` for map literals and `SET` targets, and
the session renders one stderr note. Advisories never affect rows or exit
status; Lenient/TCK sessions are untouched.

Reasons, in order of weight:

1. **The spec already answers the question.** §2: "Schema is mandatory for
   identity, **optional for shape**." A MAY-declare of some types cannot
   forbid the others without contradicting the contract; the closure was an
   accident of reachability, not a design.
2. **Closure punishes incremental typing.** Declaring one property's type
   made a label *stricter than declaring nothing* — a perverse incentive
   against exactly the gradual-typing path the workbench wants to reward.
3. **Consistency was mandatory either way** — SET being open while MATCH
   was closed satisfied nobody; closing SET would have broken shipped,
   documented behaviour that users (and our own manual) relied on.
4. **openCypher/Neo4j precedent** is open-shape; closing diverges for no
   conformance gain.
5. **Phase 10's own direction** (open predicate vocabulary, ADR-0068)
   ingests facts with arbitrary literal-valued properties; a closed shape
   would fight the phase's dogfood at the schema layer.

## Consequences

- Spec §2 gains the clarifying sentence; the manual's closure paragraph
  (added by acetone-2ck.20 while the question was open) is rewritten.
- `BindError::UnknownProperty` is removed (deep-access surface;
  snapshot re-blessed). `BoundQuery` gains `undeclared_shape_properties`.
- Type *enforcement* (ADR-0066) is untouched: a declared property's value
  is still checked at write time; seeks still trust declarations.
- Relationship property types (acetone-7qw.12) follow the same open-shape
  rule when their enforcement lands.
- A future *opt-in* closed shape (an explicit `CLOSED` declaration for
  registry-grade labels) remains open design space; nothing here precludes
  it — it would be a declared contract rather than an inference from
  partial typing.
