# ADR-0018: Diff virtual-graph query surface

*Status: accepted (Greg-chosen 2026-07-07, provisional — revisit at the Phase 4 boundary) · Date: 2026-07-07 · Bead: acetone-14c.1*

## Context

`Repository::diff(from, to)` classifies changes into `_Added`/`_Removed`/
`_Modified`. Spec §5.2 says the diff procedure yields "change rows and, in
graph form, `_Added`/… virtual elements", and §9 reserves a general
virtual-element provider interface, asking implementers to "keep `LabelScan`
provider-pluggable". But **how a query names which two versions to diff, and
in what syntax the virtual elements are reached, is not specified** — an
API-shape decision on a surface the spec wants to stay stable.

Three candidates were weighed (with Greg, 2026-07-07):

1. **Stateful `CALL … MATCH`** — `CALL acetone.diff('a','b')` mounts a diff
   context that a following `MATCH (n:_Added)` reads. Matches the bead's
   literal wording but is order-dependent: `MATCH (n:_Added)` alone would be
   meaningless, and the reader must scan upward for the context.
2. **AT-range** — `MATCH (n:_Added) AT 'a'..'b'`. Non-stateful (context on
   the clause) and the nicest surface, but stacks new `..` range grammar onto
   `AT`, which is *already* a proprietary acetone extension — doubly
   non-standard.
3. **CALL yields virtual node values** — `CALL acetone.diff('a','b') YIELD
   node` returns each changed node as a virtual value labelled with its change
   kind. No hidden state, no new grammar.

## Decision

**Option 3.** `acetone.diff` gains a `node` yield column: the changed node as
a runtime value carrying the virtual label (`_Added`/`_Removed`/`_Modified`)
*plus* its real labels, with key and record properties both queryable.

```cypher
CALL acetone.diff('v1', 'v2') YIELD node
WHERE '_Added' IN labels(node)
RETURN node.id, node.name
```

Rationale: it is the most openCypher-standard of the three (procedures that
yield node/relationship values are idiomatic Cypher — APOC's
`apoc.path.subgraphNodes() YIELD node`, etc.); the only extension is
*semantic* (a virtual label), not *syntactic*. It is non-stateful, needs no
new grammar, and reuses the procedure-provider seam from acetone-8c3 (the CLI
`RepoProcedures.acetone.diff` builds the node values via a new public
`acetone_cypher::exec::virtual_diff_node`). CLI `acetone diff` and `CALL
acetone.diff` still compute from one `Repository::diff`.

### Scope and known gaps

- **Virtual nodes only.** Relationship changes fill the row's `kind`/`label`/
  `key` columns with a null `node`; virtual *relationships* for edge changes
  are a follow-up.
- **Key properties need a declared label.** The virtual node re-exposes key
  values under their schema-declared names, so on a schemaless repo (raw
  plumbing `put-node`, no `declare-label`) the `node` carries only record
  properties — `node.id` is null. This matches the normal read path
  (`GraphSnapshot::from_records` behaves identically) and Invariant #3 makes
  schema-declared keys mandatory anyway; node *identity* is still derived from
  the key.
- **`node:_Added` predicate syntax is unavailable.** The parser accepts
  `n:Label` only in patterns and SET/REMOVE, not as a WHERE expression — a
  pre-existing gap (filed acetone-6gy). So the label test is written
  `'_Added' IN labels(node)` (standard Cypher). Adding the expression
  predicate later makes the surface `WHERE node:_Added`, benefiting all
  queries.
- **Bare `MATCH (n:_Added)`** is *not* provided. If, once this is in hand, the
  pattern form is wanted, the pluggable-`LabelScan` hook (§9) can add it over
  the same provider — but that is deferred, not baked now.

## Consequences

- The diff is queryable as a graph today, in standard Cypher, with no new
  syntax and no hidden state.
- `acetone.diff`'s declared yields grow from `(kind, label, key)` to
  `(kind, label, key, node)`; the provider tuple-width contract (assert added
  in acetone-8c3) enforces the shape.
- **Provisional and flagged for the Phase 4 boundary:** the surface is a
  reserved §9 extension-point prototype. If Greg prefers a pattern-shaped
  form (`MATCH (n:_Added) …`) after seeing this, it is an additive change over
  the same provider, not a rewrite.
- acetone-14c.1 closes, unblocking acetone-14c.4 (conflicts/`_Conflict`) and
  acetone-14c.6 (blame), which extend the same seam.
