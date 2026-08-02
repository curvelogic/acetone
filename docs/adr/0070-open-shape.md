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

1. **The spec's natural reading.** §2's "Schema is mandatory for identity,
   **optional for shape**" reads most naturally as shape-optional all the
   way down; it was, strictly, silent on whether declaring a partial shape
   closes it — this ADR amends §2 to say so explicitly. What is beyond
   argument: the closure was an accident of reachability (the rule predates
   the feature that made it fire), not a recorded design.
2. **Closure punishes incremental typing.** Declaring one property's type
   made a label *stricter than declaring nothing* — a perverse incentive
   against exactly the gradual-typing path the workbench wants to reward.
3. **Consistency was mandatory either way** — SET open while MATCH was
   closed satisfied nobody. Both behaviours shipped together in 0.4.0 the
   day before this ADR, so no reliance argument favours either side; the
   manual explicitly flagged the closure as an open question rather than a
   promise. The asymmetry itself was the bug.
4. **openCypher/Neo4j precedent** is open-shape; closing diverges for no
   conformance gain.
5. **Phase 10's own direction.** ADR-0068's open predicate vocabulary is
   stated for relationship types; the inference to properties is direct — a
   literal-valued fact lands as a node property whose name is the
   predicate — so an ingest under that model coins property names on
   demand, and a closed shape would fight the phase's dogfood at the
   schema layer. (An inference from ADR-0068, not a quotation of it.)

## Consequences

- Spec §2 gains the clarifying sentence; the manual's closure paragraph
  (added by acetone-2ck.20 while the question was open) is rewritten.
- `BindError::UnknownProperty` is removed (deep-access surface;
  snapshot re-blessed). `BoundQuery` gains `undeclared_shape_properties`.
- Type *enforcement* (ADR-0066) is untouched: a declared property's value
  is still checked at write time; seeks still trust declarations.
- Relationship property types (acetone-7qw.12) follow the same open-shape
  rule when their enforcement lands.
- **The traded-away behaviour, stated plainly**: a Strict-mode `MATCH`
  pinning a typo'd property previously failed loudly at bind; it now binds,
  matches nothing (the property exists on no node), and advises on stderr.
  Zero-rows-plus-advisory is quieter than an error — the advisory fires at
  bind time in exactly this case, and library consumers see it on
  `QueryResult::advisories`, but a caller that ignores advisories loses the
  signal. **The write side is the larger loss** for the registry use case:
  a typo'd property name on `CREATE`/`SET` is now silently persisted with
  exit status 0, and no later query surfaces it — there is not even a
  zero-rows tell. Both accepted knowingly; the natural future mitigation
  is a flag promoting shape advisories to errors, a sibling of the opt-in
  `CLOSED` declaration below.
- A future *opt-in* closed shape (an explicit `CLOSED` declaration for
  registry-grade labels) remains open design space; nothing here precludes
  it — it would be a declared contract rather than an inference from
  partial typing.
