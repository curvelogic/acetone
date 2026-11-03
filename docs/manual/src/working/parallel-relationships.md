# Parallel relationships

By default a relationship's identity is the triple *(source, type,
target)*: between one pair of nodes there is at most one `DEPENDS_ON`.
Writing a second one is refused as a duplicate, and `MERGE` finds the first.
That is the right default for most graphs, and it is what makes merges of
relationship edits well defined.

Sometimes, though, the same fact arrives more than once with different
provenance, and you want to keep each arrival: a dependency seen by a network
scanner *and* listed in a deployment manifest, a citation made in two
different review runs. acetone handles this with a **discriminator** — a
property you declare on the relationship type that becomes the fourth
component of the relationship's identity, *(source, type, target,
discriminator)*. Relationships of the type between one pair coexist as long
as their discriminator values differ. (The glossary's
[discriminator](../appendices/glossary.md) entry has the short definition.)

This chapter works through a small standalone repository rather than the
running registry, whose `DEPENDS_ON` has no discriminator. Every command and
outcome shown here is exercised by the manual's verification script.

## Declaring a discriminator

A discriminator is declared with `schema apply`, the declarative form of the
schema. The imperative `declare-rel-type` cannot express one (see
[below](#changing-the-discriminator-later)). Write a schema document naming
the type, its discriminator property and, optionally, property types — this
is the chapter's `parallel-schema.json`:

```json
{"relationship_types": [
  {"name": "DEPENDS_ON", "discriminator": "source",
   "types": {"source": "string", "seen": "string"}}
]}
```

and apply it to a fresh repository with a `Service` label:

```console
$ acetone init deps && cd deps
$ acetone declare-label Service --key name
$ acetone schema apply parallel-schema.json
relationship type "DEPENDS_ON": add
1 in the repository but not in the document: left as-is
applied 1 schema change(s)
$ acetone query "CREATE (:Service {name: 'web'}), (:Service {name: 'api'}), (:Service {name: 'db'})"
3 nodes created
$ acetone commit -m "Declare parallel DEPENDS_ON"
```

Declare the discriminator **before** relationships of the type exist. The
discriminator decides which relationships are distinct, so acetone refuses to
add, change or drop it on a declared type while relationships of that type
are stored (see
[Changing the discriminator later](#changing-the-discriminator-later)).

## Creating parallel relationships

`CREATE` takes the discriminator from the relationship's property map, like
any other property:

```console
$ acetone query "MATCH (a:Service {name: 'web'}), (b:Service {name: 'api'})
                 CREATE (a)-[:DEPENDS_ON {source: 'scanner', seen: '2026-11-01'}]->(b)"
1 relationship created
$ acetone query "MATCH (a:Service {name: 'web'}), (b:Service {name: 'api'})
                 CREATE (a)-[:DEPENDS_ON {source: 'manifest'}]->(b)"
1 relationship created
```

Two relationships now join `web` to `api`, one per source. The rules are:

- **A different value coexists.** That is the point of the declaration.
- **The same value is a duplicate.** A second `CREATE` with
  `source: 'scanner'` between the same pair is refused, exactly as a
  duplicate relationship of an undiscriminated type is:

  ```console
  $ acetone query "MATCH (a:Service {name: 'web'}), (b:Service {name: 'api'})
                   CREATE (a)-[:DEPENDS_ON {source: 'scanner'}]->(b)"
  error: cannot add the DEPENDS_ON relationship "Service" ["web"] -> "Service" ["api"]: it conflicts with an existing relationship with the same identity (source, type, target, discriminator — spec §2) — …
  ```

- **The value is mandatory.** A `CREATE` that omits the discriminator, or
  gives it as `null`, is refused:

  ```console
  $ acetone query "MATCH (a:Service {name: 'web'}), (b:Service {name: 'api'})
                   CREATE (a)-[:DEPENDS_ON]->(b)"
  error: cannot persist: relationship type "DEPENDS_ON" declares discriminator property "source": a created relationship must supply it
  ```

`MERGE` matches on the discriminator when you give it, so it is the
idempotent way to record "this source saw this dependency":

```console
$ acetone query "MATCH (a:Service {name: 'web'}), (b:Service {name: 'api'})
                 MERGE (a)-[r:DEPENDS_ON {source: 'runtime'}]->(b) RETURN r.source"
┌──────────┐
│ r.source │
├──────────┤
│ runtime  │
└──────────┘
1 row
1 relationship created
```

Running the same `MERGE` again matches the existing relationship and
creates nothing. One shape needs care: a `MERGE` that names the type but
**omits** the discriminator matches *any* existing relationship of the type
between the pair, and when none exists its create side is refused, because
the create must supply the discriminator. Give the discriminator in the
`MERGE` map whenever creation is a possible outcome.

## Reading them back

The discriminator reads back under its declared name, so it filters,
projects and sorts like any property:

```console
$ acetone query "MATCH (:Service {name: 'web'})-[r:DEPENDS_ON]->(b)
                 RETURN r.source, r.seen, b.name ORDER BY r.source"
┌──────────┬────────────┬────────┐
│ r.source │ r.seen     │ b.name │
├──────────┼────────────┼────────┤
│ manifest │ NULL       │ api    │
│ runtime  │ NULL       │ api    │
│ scanner  │ 2026-11-01 │ api    │
└──────────┴────────────┴────────┘
3 rows
```

`count(r)` counts each parallel relationship, and
`WHERE r.source = 'scanner'` selects one.

## Editing them

Every property except the discriminator is ordinary data: `SET` and
`REMOVE` work as usual. The discriminator is part of the relationship's
identity, so changing or removing it is refused, as changing a node's key
is:

```console
$ acetone query "MATCH ()-[r:DEPENDS_ON {source: 'manifest'}]->() SET r.seen = '2026-11-02'"
1 property set
$ acetone query "MATCH ()-[r:DEPENDS_ON {source: 'scanner'}]->() SET r.source = 'probe'"
error: cannot persist: cannot modify or remove discriminator property "source" of a "DEPENDS_ON" relationship (relationship identity is immutable — delete and re-create it instead)
```

To "rename" a source, delete the relationship and create it again with the
new value.

## Importing them

Edge-mode `import` takes the discriminator from a column named with
`--disc`. Given `deps.csv`:

```text
from,to,source,seen
api,db,scanner,2026-11-01
api,db,manifest,2026-10-30
```

```console
$ acetone commit -m "Record web's dependencies"
$ acetone import -f csv deps.csv --edge DEPENDS_ON --from Service=from --to Service=to --disc source
imported 0 node(s) and 2 edge(s) onto the current branch; commit …
```

The two rows become two parallel relationships from `api` to `db`, and the
`source` column reads back as `r.source`, exactly as if Cypher had
created them. `--disc` is required whenever the type declares a
discriminator: without it the import is refused before anything lands,
rather than collapsing the rows for one pair into a single relationship.

## Seeing them in diffs, reports and exports

Everything that names a relationship names its discriminator too, so
parallel relationships stay distinguishable outside Cypher. `diff` prints it
after the target:

```console
$ acetone diff HEAD~1 HEAD
+ edge "Service" ["api"] -"DEPENDS_ON"-> "Service" ["db"] ["manifest"]
+ edge "Service" ["api"] -"DEPENDS_ON"-> "Service" ["db"] ["scanner"]
```

The markdown change report prints it after the type, and the JSON report
(`report --json`) carries it as each `edges[]` entry's `disc` field:

```console
$ acetone report HEAD~1 HEAD
…
+ `Service` ["api"] DEPENDS_ON ["manifest"] → `Service` ["db"]
    - `seen`: "2026-10-30"
+ `Service` ["api"] DEPENDS_ON ["scanner"] → `Service` ["db"]
    - `seen`: "2026-11-01"
```

An edge export of a discriminated type has a `disc` column, so the export
re-imports with `import --disc disc` (prefer JSON for a faithful round trip,
as the last section explains):

```console
$ acetone export -f csv --edge DEPENDS_ON
src,dst,disc,seen
api,db,manifest,2026-10-30
api,db,scanner,2026-11-01
web,api,manifest,2026-11-02
web,api,runtime,
web,api,scanner,2026-11-01
exported 5 DEPENDS_ON edge(s)
```

Programs reading query results should project the discriminator by name
(`RETURN r.source`), as the daemon protocol notes: a whole relationship in a
JSON row carries only its type.

## Merging them

Parallel relationships merge as independent entities. Two branches that add
relationships with different discriminator values merge cleanly, both
surviving; two branches that edit the *same* relationship's property
differently conflict on that relationship alone, and the conflict names it
with its discriminator:

```console
$ acetone merge audit
merge produced 1 conflict(s):
  edge "Service" ["web"] -"DEPENDS_ON"-> "Service" ["api"] ["scanner"] property "seen"
…
```

Resolve it as any other conflict (see
[History, branching and merging](history-branch-merge.md)).

The discriminator declaration itself is merge-checked too. If one branch
declares a discriminator on a type while the other branch stores
relationships of that type, the merge cannot apply the declaration to them
safely: each such relationship surfaces as a `rel-stale-discriminator`
conflict (listed by `CALL acetone.conflicts()` and in the report) and the
merge cannot be committed until each such relationship is deleted. Re-create
them afterwards under the merged declaration, with a discriminator value
each.

## Changing the discriminator later

The discriminator is fixed once relationships of the type exist. Adding one
to a declared type in use, changing it, or dropping it would silently change
which stored relationships are distinct, so all three are refused at declare
time while relationships of the type are stored. (One gap remains, tracked as
`acetone-d5is.9`: a type that has relationships but was never declared can
still be declared *with* a discriminator. Declare the discriminator before
writing any relationships of the type.) That is also why
`declare-rel-type` cannot be used on a discriminated type: it cannot express
a discriminator, so redeclaring the type with it would drop the
discriminator, and the CLI refuses:

```console
$ acetone declare-rel-type DEPENDS_ON
error: saving workspace: cannot change relationship type "DEPENDS_ON"'s discriminator while relationships of that type exist: …
```

To change a discriminator on a type in use, delete its relationships, change
the declaration with `schema apply`, and re-create them. For a large set,
export the type as JSON first (`export -f json --edge DEPENDS_ON -o
deps.json`), edit it, and re-import it with `--disc disc`. Use JSON rather
than CSV: CSV cannot tell an absent property from an empty string, so a CSV
round trip turns absent properties into `""` (see
[Importing data end to end](importing.md)).

The [CLI reference](../reference/cli.md) documents `schema apply`,
`import --disc` and the plumbing `put-edge` (which sets no discriminator).
