# ADR-0066: A declared property type is a constraint, not an annotation

- Status: accepted
- Date: 2026-07-27
- Deciders: agent under the Phase 9 mandate (CLAUDE.md: decisions are made, not deferred). Flagged for Greg at the Phase 9 boundary because it changes the spec and the write path.
- Beads: `acetone-2ck.18` (this work), `acetone-2ck.17` (the key-seek guard that consumes it)
- Related: spec §2 (schema); ADR-0038 (typed value carriers); ADR-0065 (seek cost model); ADR-0027 (index encoding)

## Context

`acetone-2ck.18` was filed as an interface gap: `declare_label` in the CLI
passes `BTreeMap::new()` for the types map unconditionally and there is no
type flag, so a graph built entirely through the shipped CLI has no declared
property types, ever. That is true and worth fixing on its own — it is the
CLAUDE.md corollary in force, a capability not reachable through the shipped
interface is not delivered.

Reading the consumers turned up something larger. `LabelDef::types()` has
four readers:

- `graph/import.rs` — `coerce_props`/`coerce`, which coerce incoming values
  to the declared type and error when they cannot.
- `cypher/bind/catalogue.rs` and `bind/binder.rs` — Strict-mode property-name
  narrowing, advisory.
- `cypher/exec/store_source.rs` — **the seek guard**.

And `graph/constraints.rs check_nodes` validates existence and UNIQUE only;
there is no type arm. `cypher/persist.rs`, the Cypher write path, checks
existence, UNIQUE, key immutability and key value-kind — but never a declared
type. So outside import, a declared type is an assertion nothing checks.

That matters because `store_source.rs probe_value` **relies** on it. A
`Value::String` pin is served by a raw index probe only when the property's
declared type is a non-deferred scalar, because a Bytes or temporal value
compares equal to its string *rendering* at runtime (ADR-0038 carriers decay
under `eq3`) while the stored encodings differ. The declaration is the entire
basis for believing a raw probe cannot miss a row.

If the declaration can be false, the seek **under-selects**: rows that exist
and match are never visited. Candidate-superset semantics forbid exactly
that, and it is the worst failure mode available to a query engine — not an
error, but a quietly short answer. Today, against a label declaring
`os: string`, a Cypher `SET h.os = <a Bytes-valued expression>` produces it.

`acetone-2ck.17` will extend the same trust to **key** properties, where a
missed row is a missed identity.

So the interface question and the soundness question are one question. Adding
the flag alone moves CLI users from "string seeks always decline and scan" to
"string seeks fire on a promise nothing keeps".

## Decision

**A declared property type is a constraint, enforced like `UNIQUE` and
existence, at the same three points.**

1. **Write time.** `cypher/persist.rs` rejects a `CREATE`/`SET` that would
   store a value whose runtime type contradicts the label's declared type,
   before the workspace ref advances. This joins the existing checks in
   `UniqueChecker::check`, which already runs once every upserted key is
   known.
2. **Declare time.** Declaring a type over existing data that violates it is
   refused, naming the violating nodes — mirroring the `--require`/`--unique`
   backfill check `declare_label` already performs (`acetone-9gw`).
   Implemented as a type arm in `graph/constraints.rs check_nodes` rather
   than in the CLI, so it is not CLI-specific.
3. **Merge time.** `merge.rs validate_merged` gains a type arm, reporting a
   breach as a `GraphViolation::WrongType` conflict — data, not an error
   (ADR-0007) — under the same responsibility rule as existence and UNIQUE:
   report when the node changed, or when the merged schema newly declares or
   retypes the property.

   This is a *separate* implementation from (2), not a consequence of it:
   `validate_merged` does not call `check_nodes`, it re-validates the changed
   key set against the merged manifest with its own logic. An earlier draft of
   this ADR asserted merge coverage followed from putting the check in
   `check_nodes`; that was wrong, and the spec amendment below would have been
   false had it not been implemented.

   Merge is also the only point that catches a class the other two cannot:
   one branch retypes a property while the other writes a value of the old
   type. Both commits are individually legal, so no write-time check can see
   it; only the merged state breaches.

And **the CLI can declare types**: `declare-label --type <property>:<type>`,
repeatable, plus the `:declare-label` shell form, with `acetone schema`
rendering what is declared.

Null is not a type violation: an absent property is existence's business
(`REQUIRE`, ADR-0061), not the type system's. A `Value::Stored` carrier is
checked against the type it carries, not against its string rendering —
the rendering is a presentation detail of ADR-0038, and rejecting a carrier
that round-trips its own declared type would break write-back.

## Consequences

**The seek guard becomes sound rather than hopeful.** `probe_value`'s
reliance on the declared type is now backed by enforcement at every path that
can write a property. This is the precondition for `acetone-2ck.17`.

**This is a deliberate spec change, not a bug fix.** Spec §2 says a label
"MAY additionally declare property types and constraints (v0.1 supports
`UNIQUE` on non-key properties and existence constraints; **both enforced at
write time**…)". The parenthetical enumerates only UNIQUE and existence as
enforced, so unenforced types are conformant as written. §2 is amended to say
declared types are enforced on the same footing. Recorded here rather than
changed silently.

**Previously-accepted writes may now be rejected.** A graph that declared
types through the library and then wrote contradicting values through Cypher
was already relying on undefined behaviour — and, if the property was
indexed, was already getting wrong answers from seeks. No on-disk format
changes and `format_version` does not move: `types` is already part of the
encoded `SchemaEntry`. Existing repositories are readable; only writes that
would violate a declaration are refused, and `declare-label` will refuse a
type declaration that existing data already contradicts rather than
silently accepting a false one.

**Import keeps coercing.** `coerce_props` runs before validation and turns
conforming-but-differently-typed input (`"42"` for an `int`) into the
declared type, so import gains a guarantee rather than a new failure mode.

### Alternatives considered

**Leave types advisory; have the seek guard verify each fetched value's
actual type.** Rejected: it cannot work. A post-filter only sees rows the
probe visited, and under-selection is precisely the rows it did not. The
hazard is invisible from inside the seek.

**Ship the CLI flag now, enforce later.** Rejected: it is the worst ordering.
It makes the unsound path reachable by CLI users — who currently cannot
declare a type, and so are accidentally safe — and `acetone-2ck.17` then
builds on the promise while it is still unkept.

**Enforce only for indexed properties**, since that is where soundness
bites. Rejected: it makes a declaration's meaning depend on whether an index
happens to exist, so declaring an index would retroactively change which
writes are legal. A type means the same thing everywhere or it is not a type.
