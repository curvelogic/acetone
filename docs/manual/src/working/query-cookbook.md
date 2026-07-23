# Querying with Cypher: a cookbook

This chapter is a set of task-oriented recipes: a question you might ask of a
real graph, the query that answers it, and the actual output. Every command
and every line of output was produced by running acetone exactly as shown
against the [asset registry](../getting-started/asset-registry.md), in the
state that chapter leaves it — after the `decommission-app1` merge, so `app3`
exists, `app1` is empty, and postgres is at version 16.4. (If you rebuilt the
registry fresh from `asset-registry.sh` without replaying that chapter's
branch-and-merge sequence, a few outputs will differ accordingly. Commit
hashes always differ from repository to repository; everything else matches.)

acetone implements a **subset** of openCypher — the daily read/write surface
is solid, and the engine declines what it does not support with a typed error
rather than mis-executing. Where a common Cypher idiom is not yet supported,
this chapter says so, quotes the actual error, and shows the working
alternative. The [conformance appendix](../appendices/conformance.md) has the
full picture.

## Filtering: WHERE and friends

**Find services by property value.** An inline property map in the pattern
(`{tier: "core"}`) and a `WHERE` clause are equivalent for simple equality;
`WHERE` takes over as soon as you need anything more:

```console
$ acetone query 'MATCH (s:Service) WHERE s.tier = "core" RETURN s.name ORDER BY s.name'
┌──────────┐
│ s.name   │
├──────────┤
│ billing  │
│ identity │
└──────────┘
2 rows
```

**Combine conditions.** `AND`, `OR`, `NOT` and parentheses work as expected:

```console
$ acetone query 'MATCH (h:Host) WHERE h.region = "eu-west" AND h.os = "linux" RETURN h.name ORDER BY h.name'
┌────────┐
│ h.name │
├────────┤
│ app1   │
│ app3   │
│ db1    │
└────────┘
3 rows
```

**Match against a list of candidates.** `IN` checks membership; values with
no matching node simply contribute nothing (there is no error for `"ghost"`):

```console
$ acetone query 'MATCH (h:Host) WHERE h.name IN ["db1", "app3", "ghost"] RETURN h.name ORDER BY h.name'
┌────────┐
│ h.name │
├────────┤
│ app3   │
│ db1    │
└────────┘
2 rows
```

**Search within strings.** `STARTS WITH`, `ENDS WITH` and `CONTAINS` cover
most substring needs:

```console
$ acetone query 'MATCH (s:Service) WHERE s.name CONTAINS "ill" RETURN s.name'
┌─────────┐
│ s.name  │
├─────────┤
│ billing │
└─────────┘
1 row
```

Regular-expression matching (`=~`) is **not supported yet** — it declines
cleanly:

```console
$ acetone query 'MATCH (s:Service) WHERE s.name =~ "st.*" RETURN s.name'
error: line 1, column 25: regular expressions is not implemented yet
```

For the patterns a registry actually needs, the three string predicates above
are usually enough.

**Test for absent properties.** Properties are optional unless the schema
requires them, and comparing a missing property yields `NULL`, not `false` —
openCypher's three-valued logic, which acetone follows exactly. That makes a
naive "not decommissioned" filter a trap. No host in the registry has a
`decommissioned` property, so `h.decommissioned <> true` is `NULL` for every
row, and `WHERE` drops them all:

```console
$ acetone query 'MATCH (h:Host) WHERE h.decommissioned <> true RETURN h.name'
┌────────┐
│ h.name │
├────────┤
└────────┘
0 rows
```

`IS NULL` / `IS NOT NULL` is the right tool for presence tests:

```console
$ acetone query 'MATCH (h:Host) WHERE h.decommissioned IS NULL RETURN h.name ORDER BY h.name'
┌────────┐
│ h.name │
├────────┤
│ app1   │
│ app2   │
│ app3   │
│ db1    │
│ db2    │
│ edge1  │
└────────┘
6 rows
```

(Neo4j's legacy `exists(h.decommissioned)` function is not supported; use
`IS NOT NULL`.)

**Filter by label.** The label-predicate expression `WHERE n:Service` is a
known parser gap:

```console
$ acetone query 'MATCH (n) WHERE n:Service RETURN n.name'
error: line 1, column 18: expected a clause (MATCH, OPTIONAL MATCH, UNWIND, WITH, RETURN, CALL, CREATE, SET, REMOVE, DELETE or DETACH DELETE), found ':'
```

The alternative is simply to put the label in the pattern —
`MATCH (n:Service)` — which is also the faster plan, since it scans one
label instead of every node.

## Shaping results: aliases, whole nodes, DISTINCT

**Name your columns.** `AS` aliases both tidy the output and give later
clauses (`ORDER BY`, `WITH`) something to refer to:

```console
$ acetone query 'MATCH (s:Service) RETURN s.name AS service, s.tier AS tier ORDER BY service'
┌────────────┬──────┐
│ service    │ tier │
├────────────┼──────┤
│ billing    │ core │
│ identity   │ core │
│ postgres   │ data │
│ storefront │ edge │
└────────────┴──────┘
4 rows
```

**Return the whole node** when you want everything at once:

```console
$ acetone query 'MATCH (s:Service {name: "postgres"}) RETURN s'
┌────────────────────────────────────────────────────────┐
│ s                                                      │
├────────────────────────────────────────────────────────┤
│ (:Service {name: postgres, tier: data, version: 16.4}) │
└────────────────────────────────────────────────────────┘
1 row
```

**Introspect a node.** `labels(n)` and `keys(n)` (and `properties(n)`, which
returns the property map as a value) answer "what is this thing?":

```console
$ acetone query 'MATCH (s:Service {name: "postgres"}) RETURN labels(s), keys(s)'
┌───────────┬───────────────────────┐
│ labels(s) │ keys(s)               │
├───────────┼───────────────────────┤
│ [Service] │ [name, tier, version] │
└───────────┴───────────────────────┘
1 row
```

**De-duplicate.** `RETURN DISTINCT` collapses identical rows:

```console
$ acetone query 'MATCH (s:Service) RETURN DISTINCT s.tier ORDER BY s.tier'
┌────────┐
│ s.tier │
├────────┤
│ core   │
│ data   │
│ edge   │
└────────┘
3 rows
```

Map projections (`RETURN s{.name, .tier}`) are not supported; list the
properties explicitly, or return `properties(s)`.

## Ordering and paging: ORDER BY, SKIP, LIMIT

`ORDER BY` sorts (with optional `DESC`), `SKIP` discards leading rows and
`LIMIT` caps the rest — together, paging:

```console
$ acetone query 'MATCH (h:Host) RETURN h.name ORDER BY h.name SKIP 2 LIMIT 2'
┌────────┐
│ h.name │
├────────┤
│ app3   │
│ db1    │
└────────┘
2 rows
```

`SKIP` and `LIMIT` are only meaningful over a defined order — always pair
them with `ORDER BY`, or the pages you get are whatever order the match
happened to produce.

## Counting and grouping: aggregation

**Count everything, or count per group.** openCypher has no `GROUP BY`
keyword: in a `RETURN` (or `WITH`) that mixes plain expressions with
aggregates, the plain expressions *are* the grouping key. Here `s.tier` is
the group and `count(*)` counts within it:

```console
$ acetone query 'MATCH (s:Service) RETURN s.tier, count(*) ORDER BY s.tier'
┌────────┬──────────┐
│ s.tier │ count(*) │
├────────┼──────────┤
│ core   │ 2        │
│ data   │ 1        │
│ edge   │ 1        │
└────────┴──────────┘
3 rows
```

**Gather a group into a list.** `collect` is the aggregate that keeps the
values instead of counting them — one row per team, with its services:

```console
$ acetone query 'MATCH (t:Team)-[:OWNS]->(s:Service) RETURN t.name, collect(s.name) AS services ORDER BY t.name'
┌──────────┬──────────────────────┐
│ t.name   │ services             │
├──────────┼──────────────────────┤
│ payments │ [billing]            │
│ platform │ [identity, postgres] │
│ web      │ [storefront]         │
└──────────┴──────────────────────┘
3 rows
```

**Rank groups by size.** Aggregates can be aliased and sorted on like any
other column — how many hosts does each service run on, widest first:

```console
$ acetone query 'MATCH (s:Service)-[:RUNS_ON]->(h:Host) RETURN s.name, count(h) AS hosts ORDER BY hosts DESC, s.name'
┌────────────┬───────┐
│ s.name     │ hosts │
├────────────┼───────┤
│ billing    │ 2     │
│ postgres   │ 2     │
│ identity   │ 1     │
│ storefront │ 1     │
└────────────┴───────┘
4 rows
```

**Count distinct values.** `count(DISTINCT …)` answers "how many different":

```console
$ acetone query 'MATCH (h:Host) RETURN count(DISTINCT h.region) AS regions'
┌─────────┐
│ regions │
├─────────┤
│ 2       │
└─────────┘
1 row
```

**Aggregate over aggregates.** To summarise per-group counts (`sum`, `min`,
`max`, `avg`), compute the counts in a `WITH` stage and aggregate again over
its rows:

```console
$ acetone query 'MATCH (s:Service)-[:RUNS_ON]->(h:Host) WITH s, count(h) AS n RETURN sum(n) AS placements, min(n) AS fewest, max(n) AS most, avg(n) AS mean'
┌────────────┬────────┬──────┬──────┐
│ placements │ fewest │ most │ mean │
├────────────┼────────┼──────┼──────┤
│ 6          │ 1      │ 2    │ 1.5  │
└────────────┴────────┴──────┴──────┘
1 row
```

## Pipelines: WITH

`WITH` ends one stage of a query and feeds its rows to the next — it is how
you filter on an aggregate (SQL's `HAVING`), and how you narrow before
continuing to match.

**Filter on an aggregate.** Which services run on more than one host:

```console
$ acetone query 'MATCH (s:Service)-[:RUNS_ON]->(h:Host) WITH s, count(h) AS n WHERE n > 1 RETURN s.name, n ORDER BY s.name'
┌──────────┬───┐
│ s.name   │ n │
├──────────┼───┤
│ billing  │ 2 │
│ postgres │ 2 │
└──────────┴───┘
2 rows
```

**Top-N.** `WITH` also takes `ORDER BY` and `LIMIT`, so "the team owning the
most services" is one pipeline:

```console
$ acetone query 'MATCH (t:Team)-[:OWNS]->(s:Service) WITH t, count(s) AS n ORDER BY n DESC LIMIT 1 RETURN t.name, n'
┌──────────┬───┐
│ t.name   │ n │
├──────────┼───┤
│ platform │ 2 │
└──────────┴───┘
1 row
```

## When there may be nothing: OPTIONAL MATCH

A plain `MATCH` drops rows that do not match; `OPTIONAL MATCH` keeps them
and fills the unmatched variables with `NULL` — the graph equivalent of a
left outer join. Every host with whatever runs on it, *including* the empty
ones (this is where the decommissioned `app1` shows its face):

```console
$ acetone query 'MATCH (h:Host) OPTIONAL MATCH (s:Service)-[:RUNS_ON]->(h) RETURN h.name, s.name ORDER BY h.name, s.name'
┌────────┬────────────┐
│ h.name │ s.name     │
├────────┼────────────┤
│ app1   │ NULL       │
│ app2   │ billing    │
│ app3   │ billing    │
│ app3   │ identity   │
│ db1    │ postgres   │
│ db2    │ postgres   │
│ edge1  │ storefront │
└────────┴────────────┘
7 rows
```

Aggregates ignore `NULL`, so `OPTIONAL MATCH` + `count` gives an honest
per-host tally with a genuine `0` for empty hosts:

```console
$ acetone query 'MATCH (h:Host) OPTIONAL MATCH (s:Service)-[:RUNS_ON]->(h) RETURN h.name, count(s.name) AS services ORDER BY h.name'
┌────────┬──────────┐
│ h.name │ services │
├────────┼──────────┤
│ app1   │ 0        │
│ app2   │ 1        │
│ app3   │ 2        │
│ db1    │ 1        │
│ db2    │ 1        │
│ edge1  │ 1        │
└────────┴──────────┘
6 rows
```

## Walking the graph: variable-length paths

**Transitive closure.** `-[:DEPENDS_ON*]->` follows the relationship any
number of hops. Everything that ultimately depends on postgres:

```console
$ acetone query 'MATCH (s:Service)-[:DEPENDS_ON*]->(:Service {name: "postgres"}) RETURN DISTINCT s.name ORDER BY s.name'
┌────────────┐
│ s.name     │
├────────────┤
│ billing    │
│ identity   │
│ storefront │
└────────────┘
3 rows
```

`DISTINCT` matters here: the pattern matches *paths*, and a service reachable
along two different routes would otherwise appear twice.

**Bound the depth.** `*1..2` limits the walk to one or two hops — on the
registry this happens to reach the same three services, but on a deep graph
the bound is the difference between a query and an explosion:

```console
$ acetone query 'MATCH (s:Service {name: "storefront"})-[:DEPENDS_ON*1..2]->(d:Service) RETURN DISTINCT d.name ORDER BY d.name'
┌──────────┐
│ d.name   │
├──────────┤
│ billing  │
│ identity │
│ postgres │
└──────────┘
3 rows
```

A practical caveat: an unbounded `*` enumerates every distinct path, and on
densely connected graphs the number of paths grows combinatorially even when
the number of reachable nodes stays small (openCypher's no-repeated-
relationship rule keeps the walk finite, not small). Prefer an upper bound
whenever you do not need true any-depth reachability.

**See the routes themselves.** Bind the path to a variable and project it —
`nodes(p)` lists the nodes along it, `length(p)` counts the hops, and a list
comprehension turns the nodes into names:

```console
$ acetone query 'MATCH p = (:Service {name: "storefront"})-[:DEPENDS_ON*]->(:Service {name: "postgres"}) RETURN [n IN nodes(p) | n.name] AS route, length(p) AS hops ORDER BY hops, route'
┌───────────────────────────────────────────┬──────┐
│ route                                     │ hops │
├───────────────────────────────────────────┼──────┤
│ [storefront, billing, postgres]           │ 2    │
│ [storefront, identity, postgres]          │ 2    │
│ [storefront, billing, identity, postgres] │ 3    │
└───────────────────────────────────────────┴──────┘
3 rows
```

**Shortest route.** `shortestPath()` is deferred syntax:

```console
$ acetone query 'MATCH p = shortestPath((:Service {name: "storefront"})-[:DEPENDS_ON*]->(:Service {name: "postgres"})) RETURN length(p)'
error: line 1, column 11: expected '(' to open a node pattern, found 'shortestPath'
```

The working alternative — enumerate, sort by length, keep one — is fine at
workbench scale:

```console
$ acetone query 'MATCH p = (:Service {name: "storefront"})-[:DEPENDS_ON*]->(:Service {name: "postgres"}) RETURN [n IN nodes(p) | n.name] AS route ORDER BY length(p) LIMIT 1'
┌─────────────────────────────────┐
│ route                           │
├─────────────────────────────────┤
│ [storefront, billing, postgres] │
└─────────────────────────────────┘
1 row
```

(Unlike `shortestPath()`, this enumerates every path before discarding all
but one, so bound the depth on large graphs.)

## Asking about structure: pattern predicates

A relationship pattern inside `WHERE` is a predicate: "does this connection
exist?", without binding it. Services that talk to postgres *directly*:

```console
$ acetone query 'MATCH (s:Service) WHERE (s)-[:DEPENDS_ON]->(:Service {name: "postgres"}) RETURN s.name ORDER BY s.name'
┌────────────┐
│ s.name     │
├────────────┤
│ billing    │
│ identity   │
│ storefront │
└────────────┘
3 rows
```

Negated, it finds the *edges* of the dependency graph. Which service depends
on nothing (the foundation):

```console
$ acetone query 'MATCH (s:Service) WHERE NOT (s)-[:DEPENDS_ON]->() RETURN s.name'
┌──────────┐
│ s.name   │
├──────────┤
│ postgres │
└──────────┘
1 row
```

And the housekeeping classic — hosts with nothing on them, i.e. candidates
for decommissioning:

```console
$ acetone query 'MATCH (h:Host) WHERE NOT ()-[:RUNS_ON]->(h) RETURN h.name'
┌────────┐
│ h.name │
├────────┤
│ app1   │
└────────┘
1 row
```

Pattern **comprehensions** — collecting from a pattern inside an expression —
are not supported:

```console
$ acetone query 'MATCH (t:Team) RETURN t.name, [(t)-[:OWNS]->(s) | s.name] AS services'
error: line 1, column 49: expected ']' to close the list, found '|'
```

The alternative is what the [aggregation section](#counting-and-grouping-aggregation)
already showed: `MATCH` the pattern and `collect` the values.

## Strings, lists and expressions

**Take strings apart.** `split` produces a list, and lists index from zero —
extracting the zone from each host's region:

```console
$ acetone query 'MATCH (h:Host) RETURN h.name, split(h.region, "-")[1] AS zone ORDER BY h.name'
┌────────┬─────────┐
│ h.name │ zone    │
├────────┼─────────┤
│ app1   │ west    │
│ app2   │ central │
│ app3   │ west    │
│ db1    │ west    │
│ db2    │ central │
│ edge1  │ west    │
└────────┴─────────┘
6 rows
```

**The everyday functions** behave as openCypher specifies — `size` counts
characters in a string and elements in a list:

```console
$ acetone query 'RETURN toUpper("eu-west") AS shout, size("eu-west") AS chars, size(["a", "b"]) AS items'
┌─────────┬───────┬───────┐
│ shout   │ chars │ items │
├─────────┼───────┼───────┤
│ EU-WEST │ 7     │ 2     │
└─────────┴───────┴───────┘
1 row
```

**Build and transform lists.** `range` generates, and a list comprehension
filters and maps in one expression:

```console
$ acetone query 'RETURN [x IN range(1, 6) WHERE x % 2 = 0 | x * 10] AS evens'
┌──────────────┐
│ evens        │
├──────────────┤
│ [20, 40, 60] │
└──────────────┘
1 row
```

**Default missing values.** `coalesce` returns its first non-`NULL` argument
— the polite way to report optional properties (no service has an `owner`
property, so the fallback shows for all of them):

```console
$ acetone query 'MATCH (s:Service) RETURN s.name, coalesce(s.owner, "unowned") AS owner ORDER BY s.name'
┌────────────┬─────────┐
│ s.name     │ owner   │
├────────────┼─────────┤
│ billing    │ unowned │
│ identity   │ unowned │
│ postgres   │ unowned │
│ storefront │ unowned │
└────────────┴─────────┘
4 rows
```

`toInteger`, `toString`, `head`, `last`, `reverse` and friends are there too;
if a function is missing, the error says so by name (`unknown function
"exists"`), which is your cue to check the
[conformance appendix](../appendices/conformance.md).

**Branch on a value.** `CASE` maps values to values — deriving a
classification column from `tier`:

```console
$ acetone query 'MATCH (s:Service) RETURN s.name, CASE s.tier WHEN "data" THEN "stateful" ELSE "stateless" END AS kind ORDER BY s.name'
┌────────────┬───────────┐
│ s.name     │ kind      │
├────────────┼───────────┤
│ billing    │ stateless │
│ identity   │ stateless │
│ postgres   │ stateful  │
│ storefront │ stateless │
└────────────┴───────────┘
4 rows
```

## Rows from lists: UNWIND

`UNWIND` turns a list into rows, one per element — the standard way to drive
a query from a list of inputs, such as looking up a batch of hosts by name:

```console
$ acetone query 'UNWIND ["db1", "db2"] AS name MATCH (h:Host {name: name}) RETURN h.name, h.region'
┌────────┬────────────┐
│ h.name │ h.region   │
├────────┼────────────┤
│ db1    │ eu-west    │
│ db2    │ eu-central │
└────────┴────────────┘
2 rows
```

(`UNWIND` over a list of maps plus `CREATE` is the bulk-insert idiom — see
the [write recipes](#changing-the-graph-write-recipes) below.)

## Parameters

A query reads `$name` wherever a value could appear, and `--param KEY=VALUE`
(repeatable) binds it — keeping the query text fixed while the values come
from a script or a loop:

```console
$ acetone query 'MATCH (s:Service {name: $name}) RETURN s.name, s.tier' --param 'name="billing"'
┌─────────┬────────┐
│ s.name  │ s.tier │
├─────────┼────────┤
│ billing │ core   │
└─────────┴────────┘
1 row
```

The VALUE is parsed as a **Cypher literal**, with exactly the typing and
quoting the same text would have inline in a query: `42` is an integer,
`2.5` a float, `true`/`false`/`null` themselves, `"billing"` or `'billing'`
a string, and lists and maps of literals work too — a list parameter with
`IN` is the batch-lookup idiom:

```console
$ acetone query 'MATCH (h:Host) WHERE h.name IN $names RETURN h.name, h.region ORDER BY h.name' --param "names=['db1', 'db2', 'ghost']"
┌────────┬────────────┐
│ h.name │ h.region   │
├────────┼────────────┤
│ db1    │ eu-west    │
│ db2    │ eu-central │
└────────┴────────────┘
2 rows
```

Because strings must be quoted *for Cypher* as well as for your shell, the
comfortable pattern is to single-quote the whole `KEY=VALUE` and use double
quotes inside, as above. A bare unquoted word is an error, not silently a
string — a typo'd `tru` must fail loudly rather than bind the string
`"tru"` and quietly match nothing:

```console
$ acetone query 'MATCH (s:Service {name: $name}) RETURN s.name' --param name=billing
error: --param name: bare word 'billing' is not a literal — quote strings: "billing"
```

A parameter the query uses but nothing binds is still an error, as before:

```console
$ acetone query 'MATCH (s:Service {name: $name}) RETURN s.name'
error: line 1, column 25: missing parameter 'name'
```

(The converse — a `--param` the query never mentions — is accepted
silently, as in Neo4j, so one standard set of bindings can serve many
queries.)

`--param` composes with `--at`, so a parameterised lookup works against a
past version too. In the [shell](../reference/cli.md), `:param <name>
<literal>` binds a parameter for every following statement (`:param` alone
lists the current bindings, `:param-clear` drops them):

```console
$ acetone shell
acetone shell — enter queries, ':quit' to exit, ':help' for commands
acetone:main> :param name "identity"
acetone:main> MATCH (s:Service {name: $name}) RETURN s.name, s.tier;
┌──────────┬────────┐
│ s.name   │ s.tier │
├──────────┼────────┤
│ identity │ core   │
└──────────┴────────┘
1 row
```

The [library API](../reference/library-api.md) takes the same bindings as a
parameter map on `run_with`/`query_at_with`. (Interpolating values into the
query text from the shell still works, of course, but parameters spare you
the escaping — and a value that arrives in a variable never gets to
rewrite your query.)

## Changing the graph: write recipes

Writes land in the **workspace** — real and queryable immediately, history
only once you commit ([your first graph](../getting-started/first-graph.md)
covers the loop). Two things to know before experimenting:

- `checkout` refuses to switch branches over uncommitted changes, so a
  workspace full of experiments must be committed (or reverted by hand)
  before you can leave it;
- therefore: **experiment on a branch**. Your registry stays clean, and the
  experiment is one commit you never merge.

```console
$ acetone branch scratch
created branch "scratch" at 5884ee9c683c4e3b6c03e4fbdca30196cbcf4295
$ acetone checkout scratch
switched to branch "scratch"
```

**Create a node.** Remember the schema-first rule: a node's label must
declare a key ([schema and indexes](schema-and-indexes.md)), and here
`Service` also requires `tier`:

```console
$ acetone query 'CREATE (:Service {name: "search", tier: "edge", version: "0.1.0"})'
1 node created
```

**What CREATE refuses.** `CREATE` always makes a *new* node, so re-creating
an existing identity is an error, not an update — and the error tells you
the upsert idiom to use instead:

```console
$ acetone query 'CREATE (:Service {name: "search", tier: "edge"})'
error: CREATE of "Service" ["search"] conflicts with an existing node; CREATE always makes a new node, so identity collides (a MERGE on a multi-element pattern also CREATEs its nodes when the whole pattern doesn't match). To match-or-create, MERGE each node on its own before MERGEing a relationship between them: `MERGE (a:Label {…}) MERGE (b:…) MERGE (a)-[:…]->(b)`
```

Constraints are enforced at write time — a `Service` without its required
`tier` never enters the graph (note the constraint requires the property to
be *present*; it does not check its type):

```console
$ acetone query 'CREATE (:Service {name: "vault"})'
error: node "Service" ["vault"] is missing required property "tier"
```

**Upsert with MERGE.** `MERGE` on a full key pattern is the canonical
match-or-create; `ON MATCH SET` and `ON CREATE SET` say what to do in each
case. Against the existing `search` node, the `ON MATCH` branch runs:

```console
$ acetone query 'MERGE (s:Service {name: "search"}) ON CREATE SET s.version = "0.1.0" ON MATCH SET s.reviewed = true'
1 property set
```

Against a `cache` node that does not exist yet, the `ON CREATE` branch runs
instead — same statement shape, different outcome:

```console
$ acetone query 'MERGE (s:Service {name: "cache", tier: "core"}) ON CREATE SET s.version = "0.1.0" ON MATCH SET s.reviewed = true'
1 node created, 1 property set
```

**Connect-if-absent.** For relationships, `MERGE` each endpoint on its own
first, then `MERGE` the relationship between the bound variables (the idiom
the `CREATE` error above recommends — a single `MERGE` of the whole pattern
would create *everything* when any part fails to match):

```console
$ acetone query 'MERGE (s:Service {name: "search"}) MERGE (h:Host {name: "app2"}) MERGE (s)-[:RUNS_ON]->(h)'
1 relationship created
```

Run it again and it is a no-op — which is the point of `MERGE`: statements
you can safely repeat:

```console
$ acetone query 'MERGE (s:Service {name: "search"}) MERGE (h:Host {name: "app2"}) MERGE (s)-[:RUNS_ON]->(h)'
(no changes)
```

**Update and remove properties.** `SET` writes, `REMOVE` deletes a property
(both count as property writes in the summary line):

```console
$ acetone query 'MATCH (s:Service {name: "search"}) SET s.owner = "web" REMOVE s.reviewed'
2 properties set
```

One `SET` is off limits, by design: **key properties are immutable**,
because the key *is* the node's identity across every branch and version
(Load-Bearing Invariant #3). Renaming is delete-plus-create — a new
identity — or the `acetone rekey` utility:

```console
$ acetone query 'MATCH (s:Service {name: "search"}) SET s.name = "find"'
error: line 1, column 40: cannot modify key property 'name' of label 'Service' (node identity is immutable; SET/REMOVE must not touch key properties)
```

**Bulk insert.** `UNWIND` a list of maps and `CREATE` from each row:

```console
$ acetone query 'UNWIND [{name: "vault", tier: "core"}, {name: "queue", tier: "core"}] AS row CREATE (:Service {name: row.name, tier: row.tier})'
2 nodes created
```

**Delete.** A node with relationships will not silently take them with it —
`DELETE` refuses, `DETACH DELETE` deletes node and relationships together:

```console
$ acetone query 'MATCH (s:Service {name: "search"}) DELETE s'
error: line 1, column 43: invalid argument: cannot delete a node with relationships; use DETACH DELETE
$ acetone query 'MATCH (s:Service {name: "search"}) DETACH DELETE s'
1 node deleted, 1 relationship deleted
```

**Wrap up the experiment.** The workspace is dirty, and `checkout` says so:

```console
$ acetone status
On branch scratch
HEAD: 5884ee9c683c4e3b6c03e4fbdca30196cbcf4295
workspace: dirty
nodes: 16, edges: 15, schema entries: 7
$ acetone checkout main
error: checking out branch "main": workspace has uncommitted changes; commit them first
```

Commit on the scratch branch, switch back, and confirm `main` never saw any
of it — the four original services, untouched:

```console
$ acetone commit -m "cookbook: scratch experiments"
committed fb6382c4fe97ac8d1c87a93a74bce1b7b28c9f7e
$ acetone checkout main
switched to branch "main"
$ acetone query 'MATCH (s:Service) RETURN count(*) AS services'
┌──────────┐
│ services │
├──────────┤
│ 4        │
└──────────┘
1 row
```

## Querying history

The version-control side of acetone is queryable from Cypher too — history
is data. (Remember: your commit hashes will differ from the ones printed
here; take yours from `acetone log` or the procedure below.)

**List commits.** `CALL acetone.log()` yields the current branch's history
as rows — and, like any `CALL`, composes with the rest of the language via
`YIELD`:

```console
$ acetone query 'CALL acetone.log()'
┌──────────────────────────────────────────┬───────────────────────────────────┐
│ commit                                   │ subject                           │
├──────────────────────────────────────────┼───────────────────────────────────┤
│ 5884ee9c683c4e3b6c03e4fbdca30196cbcf4295 │ merge decommission-app1           │
│ fd9fcdbbd81a7d4ad587682be2cc3cb10067747c │ postgres upgraded to 16.4         │
│ 7b7c4223b1d9161d0f1c1fbd573ad5738f83bd33 │ asset registry: initial inventory │
└──────────────────────────────────────────┴───────────────────────────────────┘
3 rows
$ acetone query 'CALL acetone.log() YIELD commit, subject RETURN subject LIMIT 2'
┌───────────────────────────┐
│ subject                   │
├───────────────────────────┤
│ merge decommission-app1   │
│ postgres upgraded to 16.4 │
└───────────────────────────┘
2 rows
```

**Time travel.** Two equivalent spellings. The `--at` flag runs the whole
query against a past version — postgres's version at the seed commit:

```console
$ acetone query --at 7b7c4223b1d9161d0f1c1fbd573ad5738f83bd33 'MATCH (s:Service {name: "postgres"}) RETURN s.version'
┌───────────┐
│ s.version │
├───────────┤
│ 16.3      │
└───────────┘
1 row
```

Or in the language itself: `AT "<refspec>"` after a `MATCH` reads that
pattern at the named branch, tag or commit:

```console
$ acetone query 'MATCH (s:Service {name: "postgres"}) AT "7b7c4223b1d9161d0f1c1fbd573ad5738f83bd33" RETURN s.version'
┌───────────┐
│ s.version │
├───────────┤
│ 16.3      │
└───────────┘
1 row
```

Branch names work as refspecs too — from `main`, peek at what the scratch
branch committed without checking it out (four original services plus
`cache`, `vault` and `queue`):

```console
$ acetone query 'MATCH (s:Service) AT "scratch" RETURN count(*) AS services'
┌──────────┐
│ services │
├──────────┤
│ 7        │
└──────────┘
1 row
```

Git *ancestry* refspecs are not resolved yet — spell the commit out by hash
instead:

```console
$ acetone query --at main~1 'MATCH (s:Service {name: "postgres"}) RETURN s.version'
error: cannot resolve "main~1" to a tag, branch, ref or commit
```

**Diff as rows.** `CALL acetone.diff(from, to)` yields the graph-level
difference — the same information as `acetone diff`, but as data you can
filter, count or join. Note the `_Added`/`_Modified` virtual labels on the
node column:

```console
$ acetone query 'CALL acetone.diff("7b7c4223b1d9161d0f1c1fbd573ad5738f83bd33", "main")'
┌──────────┬─────────┬─────────────────────────────────────────────────────┬──────────────────────────────────────────────────────────────────┐
│ kind     │ label   │ key                                                 │ node                                                             │
├──────────┼─────────┼─────────────────────────────────────────────────────┼──────────────────────────────────────────────────────────────────┤
│ added    │ Host    │ "Host" ["app3"]                                     │ (:_Added:Host {name: app3, os: linux, region: eu-west})          │
│ modified │ Service │ "Service" ["postgres"]                              │ (:_Modified:Service {name: postgres, tier: data, version: 16.4}) │
│ removed  │ RUNS_ON │ "Service" ["billing"] -"RUNS_ON"-> "Host" ["app1"]  │ NULL                                                             │
│ added    │ RUNS_ON │ "Service" ["billing"] -"RUNS_ON"-> "Host" ["app3"]  │ NULL                                                             │
│ removed  │ RUNS_ON │ "Service" ["identity"] -"RUNS_ON"-> "Host" ["app1"] │ NULL                                                             │
│ added    │ RUNS_ON │ "Service" ["identity"] -"RUNS_ON"-> "Host" ["app3"] │ NULL                                                             │
└──────────┴─────────┴─────────────────────────────────────────────────────┴──────────────────────────────────────────────────────────────────┘
6 rows
```

**Blame.** Which commits touched an entity — `CALL acetone.blame(label,
key)` walks history for one node (for the single-property keys used
throughout this manual, pass the key value directly). Postgres was written
by the seed commit and the 16.4 upgrade:

```console
$ acetone query 'CALL acetone.blame("Service", "postgres")'
┌─────────┬──────────┬──────────────────────────────────────────┐
│ label   │ key      │ commit                                   │
├─────────┼──────────┼──────────────────────────────────────────┤
│ Service │ postgres │ fd9fcdbbd81a7d4ad587682be2cc3cb10067747c │
│ Service │ postgres │ 7b7c4223b1d9161d0f1c1fbd573ad5738f83bd33 │
└─────────┴──────────┴──────────────────────────────────────────┘
2 rows
```

**Conflicts.** During a merge that stopped on conflicts, `CALL
acetone.conflicts()` lists each conflicted key with its base/ours/theirs
values. With no merge in progress it is simply empty — the
[history chapter](history-branch-merge.md) drives a real conflict through
this procedure:

```console
$ acetone query 'CALL acetone.conflicts()'
┌───────┬─────┬──────────┬──────┬──────┬────────┬──────┐
│ label │ key │ property │ base │ ours │ theirs │ node │
├───────┼─────┼──────────┼──────┼──────┼────────┼──────┤
└───────┴─────┴──────────┴──────┴──────┴────────┴──────┘
0 rows
```

## Output for scripts: JSON and CSV

`--format json` emits one object per row — lists stay lists, which makes
`jq` pipelines pleasant:

```console
$ acetone query --format json 'MATCH (t:Team)-[:OWNS]->(s:Service) RETURN t.name, collect(s.name) AS services ORDER BY t.name'
[
  {"t.name": "payments", "services": ["billing"]},
  {"t.name": "platform", "services": ["identity", "postgres"]},
  {"t.name": "web", "services": ["storefront"]}
]
```

`--format csv` writes a header row and the data, ready for a spreadsheet or
the next tool in the pipe:

```console
$ acetone query --format csv 'MATCH (s:Service)-[:RUNS_ON]->(h:Host) RETURN s.name, h.name ORDER BY s.name, h.name'
s.name,h.name
billing,app2
billing,app3
identity,app3
postgres,db1
postgres,db2
storefront,edge1
```

## Where next

That is the working vocabulary: match, filter, shape, aggregate, pipeline,
walk, write, and query history itself. The
[import chapter](importing.md) feeds the registry from files instead of
hand-written `CREATE`s; the
[history chapter](history-branch-merge.md) goes deep on branching, merging
and conflicts; and when a query declines with "not supported", the
[conformance appendix](../appendices/conformance.md) tells you whether it is
a gap, a deferral or a design decision.
