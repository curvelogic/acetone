# Schema and indexes

acetone's schema is **mandatory for identity, optional for shape**. Every
primary label must declare a key — the ordered tuple of properties that gives
its nodes their identity — before a single node of that label can exist.
Everything else (existence constraints, uniqueness, indexes) is opt-in. This
chapter works through what those declarations mean in practice, what they
protect you from, and how they behave on a graph that already has data and
history.

It continues from the end of the
[history chapter](history-branch-merge.md); the registry's schema at this
point is unchanged since Part I:

```console
$ acetone schema
Labels
  "Host"     key ("name")
  "Service"  key ("name")  required ("tier")
  "Team"     key ("name")
Relationship types
  "DEPENDS_ON"
  "OWNS"
  "RUNS_ON"
Indexes
  "host_by_region"  on "Host" ("region")
```

## Natural keys are identity

The pair `(primary label, key values)` — `("Host", ["db1"])` — is not merely
a lookup convenience. It is the node's **identity** for storage, for `diff`,
and for `merge`. When the history chapter's merge combined two branches'
edits to identity, it knew they were edits to *the same node* because both
sides addressed `("Service", ["identity"])`. There are no internal node IDs
to drift apart between branches; the key is the whole story. This is
load-bearing (Invariant #3 in the project's terms), and the write surface
defends it from every angle.

`CREATE` always means "a new node", so creating a key that exists is an
error, not an update:

```console
$ acetone query 'CREATE (:Host {name: "db1", region: "eu-west", os: "linux"})'
error: CREATE of "Host" ["db1"] conflicts with an existing node; CREATE always makes a new node, so identity collides (a MERGE on a multi-element pattern also CREATEs its nodes when the whole pattern doesn't match). To match-or-create, MERGE each node on its own before MERGEing a relationship between them: `MERGE (a:Label {…}) MERGE (b:…) MERGE (a)-[:…]->(b)`
```

`SET` and `REMOVE` must never touch key properties — a node cannot quietly
become a different node:

```console
$ acetone query 'MATCH (h:Host {name: "db1"}) SET h.name = "db01"'
error: line 1, column 34: cannot modify key property 'name' of label 'Host' (node identity is immutable; SET/REMOVE must not touch key properties)
$ acetone query 'MATCH (s:Service {name: "billing"}) REMOVE s.name'
error: line 1, column 44: cannot modify key property 'name' of label 'Service' (node identity is immutable; SET/REMOVE must not touch key properties)
```

### When a thing really is renamed: `rekey`

Real things do get renamed, and for that there is `acetone rekey`. A key
change is modelled honestly as **delete-plus-create** — a new identity —
recorded in one commit, with every incident edge rewritten onto the new key:

```console
$ acetone rekey Team web webshop -m "team web renamed to webshop"
rekeyed "Team" ["web"] -> "Team" ["webshop"] in 19a5454a9271aec1c51f50535a6119b321881b96
$ acetone query 'MATCH (t:Team {name: "webshop"})-[:OWNS]->(s:Service) RETURN s.name'
┌────────────┐
│ s.name     │
├────────────┤
│ storefront │
└────────────┘
1 row
```

Being just a commit, undoing it is the same operation the other way:

```console
$ acetone rekey Team webshop web -m "revert: keep the team name web"
rekeyed "Team" ["webshop"] -> "Team" ["web"] in 1487065ec7ecaa45297ca5cb0cc333fc6e1ef672
```

(One consequence worth understanding: to `diff`, `blame` and `merge`, the
rekeyed node's history starts afresh — the old key's trail ends, the new
key's begins. That is the honest reading of "identity changed". `rekey`
currently handles single-column keys.)

### Match-or-create: `MERGE`

The canonical upsert is `MERGE` on a full key pattern: match the node if the
key exists, create it if not, with `ON CREATE SET`/`ON MATCH SET` for the
two outcomes. Run twice, it takes each path in turn:

```console
$ acetone query 'MERGE (h:Host {name: "edge2"}) ON CREATE SET h.region = "us-east", h.os = "linux" ON MATCH SET h.os = "linux-6.9"'
1 node created, 2 properties set
$ acetone query 'MERGE (h:Host {name: "edge2"}) ON CREATE SET h.region = "us-east", h.os = "linux" ON MATCH SET h.os = "linux-6.9"'
1 property set
$ acetone query 'MATCH (h:Host {name: "edge2"}) RETURN h.region, h.os'
┌──────────┬───────────┐
│ h.region │ h.os      │
├──────────┼───────────┤
│ us-east  │ linux-6.9 │
└──────────┴───────────┘
1 row
```

This idempotence is what makes re-imports clean diffs — the
[import chapter](importing.md) leans on it heavily.

## Composite keys and constraints

Part I's labels all have single-property keys. `--key` repeats for a
**composite** key, in order. Let's extend the registry downwards, to network
interfaces — a thing whose natural identity is *which host* plus *which
interface*, and which carries two shape constraints besides: every interface
must record an MTU, and MAC addresses must be unique:

```console
$ acetone declare-label Interface --key host --key name --require mtu --unique mac
declared label "Interface" key ["host", "name"]
```

A node satisfying all of that goes in without comment:

```console
$ acetone query 'CREATE (:Interface {host: "db1", name: "eth0", mtu: 9000, mac: "02:42:0a:00:00:01"})'
1 node created
```

The constraints are enforced **at write time**. A missing required property:

```console
$ acetone query 'CREATE (:Interface {host: "db1", name: "eth1", mac: "02:42:0a:00:00:02"})'
error: node "Interface" ["db1", "eth1"] is missing required property "mtu"
```

A duplicated unique property:

```console
$ acetone query 'CREATE (:Interface {host: "db1", name: "eth1", mtu: 1500, mac: "02:42:0a:00:00:01"})'
error: UNIQUE constraint on "Interface"."mac" violated: value already used by another node
```

And done properly, both pass. Note the composite key working: a second
`eth0` is fine on a *different* host, because the identity is the pair:

```console
$ acetone query 'CREATE (:Interface {host: "db1", name: "eth1", mtu: 1500, mac: "02:42:0a:00:00:02"})'
1 node created
$ acetone query 'CREATE (:Interface {host: "db2", name: "eth0", mtu: 1500, mac: "02:42:0a:00:00:03"})'
1 node created
```

That last command also demonstrates something by *not* failing: the registry
retired host `db2` in the previous chapter, and nothing objected to an
interface claiming `host: "db2"`. A key property is a plain value — **not a
foreign key**. If you want the graph to guarantee the host exists, say it
with an edge, which brings us to relationship types. They must be declared
too (no options — just the name):

```console
$ acetone query 'MATCH (i:Interface {host: "db1", name: "eth0"}), (h:Host {name: "db1"}) CREATE (i)-[:ON_HOST]->(h)'
error: line 1, column 83: unknown relationship type "ON_HOST" — declare it first with `acetone declare-rel-type "ON_HOST"`
$ acetone declare-rel-type ON_HOST
declared relationship type "ON_HOST"
$ acetone query 'MATCH (i:Interface {host: "db1", name: "eth0"}), (h:Host {name: "db1"}) CREATE (i)-[:ON_HOST]->(h)'
1 relationship created
```

An `ON_HOST` edge *is* referentially guarded: delete the host and the merge
and commit machinery will flag the dangling edge, exactly as in the
[history chapter](history-branch-merge.md).

## Changing schema on a populated graph

Schema declarations are ordinary workspace writes: they take effect
immediately, travel through `commit`, `diff` and `merge` like data, and are
versioned with everything else. Three rules govern changing them once data
exists.

**1. A populated label's key cannot change.** Identity is immutable at the
label level too:

```console
$ acetone declare-label Host --key serial
error: saving workspace: cannot change the key of label "Host": nodes already exist under its current key (node identity is immutable — redeclare before adding data, or use migrate)
```

**2. Redeclaring a label replaces its whole constraint set.** `declare-label`
is declarative, not additive: the label's constraints become exactly what the
latest declaration says. To add a constraint, restate the existing ones
alongside it; a declaration that omits them drops them.

**3. Retrofitted constraints are validated against the existing data.** A
constraint declared over data that already violates it is refused outright,
with every violating node named — it does not become a landmine that
detonates on the next unrelated write. Watch what happens when we require a
`slack` property of teams that do not have one:

```console
$ acetone declare-label Team --key name --require slack
error: cannot declare label "Team": existing data violates the declared constraints — 3 constraint violations:
  node "Team" ["payments"] is missing required property "slack"
  node "Team" ["platform"] is missing required property "slack"
  node "Team" ["web"] is missing required property "slack"
```

The schema is unchanged — the refusal staged nothing — so ordinary writes
carry on as before. The error is the to-do list: **backfill first, declare
after**. Supply the property, and the same declaration is accepted:

```console
$ acetone query 'MATCH (t:Team) SET t.slack = t.oncall'
3 properties set
$ acetone declare-label Team --key name --require slack
declared label "Team" key ["name"]
```

(`--unique` retrofits are validated the same way: a duplicated value among
existing nodes is refused with the colliding nodes named.) Here we drop the
experiment, which also demonstrates rule 2 — restating the label without
`--require slack` removes the constraint:

```console
$ acetone declare-label Team --key name
declared label "Team" key ["name"]
```

## Indexes

A declared index accelerates equality lookups on a property — Part I's
`host_by_region` is one. `--property` repeats for a **composite** index over
an ordered tuple of properties. Declare one for the new label, and a
composite one for hosts:

```console
$ acetone declare-index if_by_mac --label Interface --property mac
declared index "if_by_mac" on "Interface"("mac")
$ acetone declare-index host_by_region_os --label Host --property region --property os
declared index "host_by_region_os" on "Host"("region", "os")
```

Nothing about your queries changes — the planner simply uses an index when
one covers the predicate:

```console
$ acetone query 'MATCH (h:Host {region: "eu-west", os: "linux"}) RETURN h.name ORDER BY h.name'
┌────────┐
│ h.name │
├────────┤
│ app1   │
│ app3   │
│ db1    │
└────────┘
3 rows
```

Indexes are **derived data**: built from the nodes map at declaration time,
maintained transactionally with every write thereafter, and — by another
load-bearing invariant — exactly reproducible from their sources.
`acetone reindex` rebuilds every declared index and must produce
byte-identical results; it is a no-op on a healthy repository and the repair
for any index divergence `fsck` reports:

```console
$ acetone reindex
reindexed
```

Two small print items: indexes are null- and NaN-blind (a node without the
property simply is not in the index), and equality is what they serve —
range queries fall back to scans in this release.

## Schema is versioned too

Because schema lives in the same commits as data, history queries resolve
against the schema *of the version being queried*. Ask a time-travelled
question about a label that did not exist then, and it is not "zero rows" —
it is not a label at all:

```console
$ acetone query --at refs/tags/inventory-v1 'MATCH (i:Interface) RETURN count(i)'
error: line 1, column 7: unknown label "Interface" (not declared in the schema)
```

(`acetone schema` takes `--at` too, printing the declarations of exactly
that version. The `schema entries` count in `acetone status` is a quick
check of how many declarations a version carries.)

Everything this chapter added is still sitting in the workspace; seal it:

```console
$ acetone status
On branch main
HEAD: 1487065ec7ecaa45297ca5cb0cc333fc6e1ef672
workspace: dirty
nodes: 16, edges: 15, schema entries: 11
$ acetone commit -m "interface inventory: schema and first entries"
committed 9a85a178c76bf71268ece2e6e327ed470f5af9e2
$ acetone schema
Labels
  "Host"       key ("name")
  "Interface"  key ("host", "name")  required ("mtu")  unique ("mac")
  "Service"    key ("name")  required ("tier")
  "Team"       key ("name")
Relationship types
  "DEPENDS_ON"
  "ON_HOST"
  "OWNS"
  "RUNS_ON"
Indexes
  "host_by_region"     on "Host" ("region")
  "host_by_region_os"  on "Host" ("region", "os")
  "if_by_mac"          on "Interface" ("mac")
```

The registry now has eleven schema entries and its first sub-host inventory.
Next: keeping the repository itself healthy, in
[maintenance and migration](maintenance-and-migration.md).
