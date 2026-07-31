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
existence — at their three points, plus a fourth that is the load-bearing
one.**

**0. Save time — the universal chokepoint.**
`Transaction::save_in_place` validates every staged node record against the
transaction's final schema (`check_staged_node_types`), raising
`GraphError::PropertyTypeViolation`. This is listed first because it is what
makes the claim below true: the Cypher write path is *not* the only writer.
`acetone put-node`, `rekey`, import, `resolve` and any library caller stage
records straight onto a `Transaction`. Enforcing only in `persist` left
`put-node` able to store a value contradicting its declaration — found and
fixed within this bead, and the reason this entry exists.

`apply_map`, which writes the `nodes` map, is called only from
`save_in_place`, which is reached only from `save()` and `commit()`, so every
`Transaction`-side write is covered. The merge path writes a merged manifest
directly rather than through a `Transaction`, which is exactly why it needs
its own validator (3). The schema map is applied *before* the node map so the
check sees the transaction's own schema ops — declaring a type and writing
conforming values in one session must work.

Staged ops are validated by reference before being consumed, so a large
batched import allocates nothing extra (ADR-0062's bounded-memory promise
holds). The schema scan is skipped when no node put is staged, and record
decoding is skipped when no label declares a type.

**Cost, measured rather than asserted** — a phase whose subject is a seek cost
model should not add an unmeasured per-write cost. Importing 20,000 CSV rows,
comparing a label declaring a type on a property the data never carries
(identical stored records, so this isolates the schema scan and the
per-record decode) against the same label with no declaration: the declared
side was **not slower in any run**, and the difference sat below run-to-run
variance — the same configuration varied 0.18 s to 0.86 s across batches on a
loaded machine, swamping any effect. The work is O(staged) + O(schema), and
the schema map is a handful of entries, so this is the expected result rather
than a surprising one. It is not a claim that the cost is zero; it is a claim
that it is not measurable at this scale by this method.

One extension point is deliberately outside this: `migrate::FormatTransform`
is a public trait that rewrites manifests without a `Transaction`. Today's
`Rechunk` copies entries verbatim so it cannot introduce a breach, but a
future transform could.

The three points that mirror the existing constraints:

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

   **Amended by `acetone-2ck.17`.** As first shipped this was CLI-specific in
   practice: `constraints::check_label` had exactly one caller, the CLI's
   `declare_label`, while `Transaction::put_schema` — public, and part of the
   frozen 0.2 surface — installed a declaration with no backfill check at
   all, and `check_staged_node_types` returns early when a transaction stages
   no node put. So a schema-only transaction could retype *around* existing
   data and make the declaration false the moment it landed. That was not
   theoretical: a fresh repository built through the public library, with a
   `String`-keyed and a `Bytes`-keyed node under one label, then declared
   `id: string`, answered `MATCH (b:Blob {id: 'deadbeef'})` with one row
   where the scan spelling returned two — a silent wrong answer on node
   identity, found by the reviewer of `acetone-2ck.17`. `save_in_place` now
   runs `check_retyped_labels` on any schema change, over the labels whose
   declared types changed and the properties that changed, skipping nodes the
   same transaction rewrites *or deletes* so a retype and its backfill can
   land together — deletion matters, because a **key** property cannot be
   repaired by rewriting a record, leaving delete-and-recreate as the only
   route. It costs a prefix scan of the retyped label, on a schema change
   only: the same work `declare-label` already did, moved from the CLI to the
   primitive every writer passes through — and cheaper, by more than a
   constant. `constraints::check_label` walks `snapshot.nodes()`, every node
   in the graph, and materialises the matching ones into a `NodeSet`;
   `check_retyped_labels` prefix-scans the one label and streams it. (An earlier draft of this amendment said the
   scan "was already paid for" by `check_label_key_stability`. That was
   wrong — that check scans nothing unless a key *tuple* changed, and then
   only probes for a single node.) Spec §2's parenthetical licensing the gap
   is removed, and replaced by one naming the gap that genuinely remains.
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

**One widening is allowed: `float` admits an integer.** It is lossless, and
it is what the import path already does — `import::coerce` turns an `Int`
into a `Float` for a `float` declaration rather than failing. Without it the
two enforcement paths would disagree about the same value: `SET n.ratio = 1`
refused on a `float` property while importing that same `1` succeeded.
Integer literals for float-valued properties are ordinary Cypher, so refusing
them would be a wart with no soundness benefit — the seek probes both numeric
encodings regardless (`probe_value`), so nothing downstream depends on a
`float`-declared property holding only floats.

The reverse is **not** allowed. Narrowing a float to an integer is lossy
(`80.5` has no integer form), import rejects it, and admitting it here would
reintroduce the disagreement in the other direction.

A consequence worth stating: mixed `Int`/`Float` values under one property
remain reachable under a **`float`** declaration (by the widening above) and
under **no** declaration, but not under `int`. The store-backed seek must
still probe both numeric encodings in both of those cases, and the
`store_source` composite fixture — which seeds `Int(80)` and `Float(80.0)`
under one property — was retargeted from `int` onto an untyped property to
stay legal.

That retargeting is a **larger loosening than enforcement required**:
declaring the property `float` would have kept the mixed values legal *and*
kept a declared type on the component. It also leaves one arm uncovered — a
string pin on a declared component at a non-zero position, the positive case
of the guard `probe_value` applies per component. Recorded here rather than
silently accepted; the follow-up is `acetone-2ck.20`.

**Key properties are covered too, and they are the subtle half.** A node's key
values live in its `NodeKey`, not its `NodeRecord`, so the three record-based
checks (0, 2, 3) initially exempted exactly the properties whose type matters
most — `acetone-2ck.17` extends the seek guard's trust to key properties,
where a missed row is a missed identity. All three now share
`constraints::type_violations`, which reads a key property's value from the
key tuple by position and everything else from the record, so they cannot
diverge. The existence check has the same shape for the same reason.

## Consequences

**The seek guard becomes sound rather than hopeful.** `probe_value`'s
reliance on the declared type is now backed by enforcement at every path that
can *write* a property. This is the precondition for `acetone-2ck.17`.

**One readable state is still not covered, and it is a read, not a write.** A
merge that produces graph-level violations persists the merged manifest as the
workspace and rebuilds its indexes over the merged nodes before
`validate_merged` runs. Commit is refused, so nothing reaches history — but
that workspace is queryable, and a seek over an index on the violating
property can under-select there. Conflicts are data rather than errors
(ADR-0007), so no write-time check gates it by construction; closing it means
changing the *read* path, which is out of this bead's scope. Filed as
`acetone-7qw.14` with the options set out. Until then, "enforcement at every
path that can write a property" should be read literally: it is a claim about
writers, not about every state a reader can observe.

**This is a deliberate spec change, not a bug fix.** Spec §2 says a label
"MAY additionally declare property types and constraints (v0.1 supports
`UNIQUE` on non-key properties and existence constraints; **both enforced at
write time**…)". The parenthetical enumerates only UNIQUE and existence as
enforced, so unenforced types are conformant as written. §2 is amended to say
declared types are enforced on the same footing. Recorded here rather than
changed silently.

**Four public enums gain a variant, and only one was visible to the freeze
gate.** `PersistError::WrongType` (acetone-cypher) tripped the ADR-0046
snapshot and was deliberately re-blessed. `GraphError::PropertyTypeViolation`,
`ConstraintViolation::WrongType` and `GraphViolation::WrongType` did **not**:
`crates/acetone-core/public-api.txt` records only the re-export line for
`acetone_core::graph`, not the variants behind it, so the gate is blind to
them (`acetone-7qw.5`, which this work turned from a removals concern into a
demonstrated additions one too). None of the four enums is
`#[non_exhaustive]`, so each addition breaks a downstream exhaustive match —
permitted within a patch series by STABILITY.md's additive-only rule, but
worth stating plainly rather than resting on a green gate that could not have
failed. `acetone-fht` proposes `#[non_exhaustive]` for `GraphError` at the
next minor boundary.

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
