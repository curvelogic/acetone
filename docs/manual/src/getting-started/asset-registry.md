# The asset registry: a running example

acetone's founding use case is an **asset registry**: an inventory of
infrastructure — what exists, where it runs, who owns it, what depends on
what — where change arrives through pull–edit–push workflows or logged
imports, and where full, queryable history is a first-class requirement. This
chapter builds a small asset registry that the rest of the manual reuses: the
[query cookbook](../working/query-cookbook.md), the
[import walkthrough](../working/importing.md), the
[history and merging chapter](../working/history-branch-merge.md) and the
[recovery runbook](../recovery/runbook.md) all work against this dataset.

It is small enough to read in one sitting and rich enough to be worth
querying: three teams own four services, which run on five hosts and depend on
one another.

## The shape

Three node labels and three relationship types:

```text
  Team ──OWNS──▶ Service ──RUNS_ON──▶ Host
                    │
                    └──DEPENDS_ON──▶ Service
```

The service dependency graph:

```text
  storefront ──▶ billing ──▶ identity ──▶ postgres
       │            │                        ▲
       │            └────────────────────────┘
       └─────────▶ identity
```

| Label | Key | Other properties | Instances |
|---|---|---|---|
| `Team` | `name` | `oncall` | platform, payments, web |
| `Service` | `name` | `tier` (required), `version` | postgres, identity, billing, storefront |
| `Host` | `name` | `region` (indexed), `os` | db1, db2, app1, app2, edge1 |

The schema declarations demonstrate three features beyond plain keys:

- `--require tier` on `Service` is an **existence constraint** — a `Service`
  without a `tier` is rejected at write time:

  ```console
  $ acetone query 'CREATE (:Service {name: "search"})'
  error: node "Service" ["search"] is missing required property "tier"
  ```

- `declare-index host_by_region` declares a **property index** — equality
  lookups on `Host.region` are served from the index rather than a scan.
- Keys here are all single properties, but `--key` repeats for composite
  keys; see [schema and indexes](../working/schema-and-indexes.md).

## Building it

The whole registry is built by the script below — also available as
[`asset-registry.sh`](asset-registry.sh) next to this page. Run it in an
empty directory with `acetone` on your `PATH`; it is exactly the workflow of
[the previous chapter](first-graph.md), scaled up: init, declare, create,
commit once.

```sh
{{#include asset-registry.sh}}
```

Running it prints each step's confirmation and ends with the seed commit:

```console
$ mkdir registry && cd registry
$ sh asset-registry.sh
Initialized empty acetone repository in .
declared label "Team" key ["name"]
...
committed 36ecda1611ec97ca85af3a36eb8b9b960dd9b0c1
$ acetone status
On branch main
HEAD: 36ecda1611ec97ca85af3a36eb8b9b960dd9b0c1
workspace: clean
nodes: 12, edges: 15, schema entries: 7
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

(Your commit hashes will differ from the ones printed in this chapter —
commits include timestamps. Everything else will match.)

## A tour in six queries

**Who owns what.** Ownership is an edge, so the answer is a one-hop match:

```console
$ acetone query 'MATCH (t:Team)-[:OWNS]->(s:Service) RETURN t.name, s.name ORDER BY t.name, s.name'
┌──────────┬────────────┐
│ t.name   │ s.name     │
├──────────┼────────────┤
│ payments │ billing    │
│ platform │ identity   │
│ platform │ postgres   │
│ web      │ storefront │
└──────────┴────────────┘
4 rows
```

**Where does billing run?**

```console
$ acetone query 'MATCH (s:Service {name: "billing"})-[:RUNS_ON]->(h:Host) RETURN h.name, h.region ORDER BY h.name'
┌────────┬────────────┐
│ h.name │ h.region   │
├────────┼────────────┤
│ app1   │ eu-west    │
│ app2   │ eu-central │
└────────┴────────────┘
2 rows
```

**What depends on postgres, directly?** Traverse the `DEPENDS_ON` edge
backwards:

```console
$ acetone query 'MATCH (s:Service)-[:DEPENDS_ON]->(:Service {name: "postgres"}) RETURN s.name ORDER BY s.name'
┌──────────┐
│ s.name   │
├──────────┤
│ billing  │
│ identity │
└──────────┘
2 rows
```

**Everything storefront depends on, transitively.** A variable-length path
(`*`) walks the dependency graph to any depth:

```console
$ acetone query 'MATCH (s:Service {name: "storefront"})-[:DEPENDS_ON*]->(d:Service) RETURN DISTINCT d.name ORDER BY d.name'
┌──────────┐
│ d.name   │
├──────────┤
│ billing  │
│ identity │
│ postgres │
└──────────┘
3 rows
```

**Hosts by region** — an equality lookup on the indexed property:

```console
$ acetone query 'MATCH (h:Host {region: "eu-west"}) RETURN h.name, h.os ORDER BY h.name'
┌────────┬─────────┐
│ h.name │ h.os    │
├────────┼─────────┤
│ app1   │ linux   │
│ db1    │ linux   │
│ edge1  │ freebsd │
└────────┴─────────┘
3 rows
```

**The blast radius.** If host `db1` dies, which services are affected —
including everything that transitively depends on them — and which on-call
channel do you page? One query joins placement, the dependency closure
(`*0..` includes the service on the host itself) and ownership:

```console
$ acetone query 'MATCH (h:Host {name: "db1"})<-[:RUNS_ON]-(s:Service)<-[:DEPENDS_ON*0..]-(affected:Service)<-[:OWNS]-(t:Team) RETURN DISTINCT affected.name, t.name, t.oncall ORDER BY affected.name'
┌───────────────┬──────────┬──────────────────┐
│ affected.name │ t.name   │ t.oncall         │
├───────────────┼──────────┼──────────────────┤
│ billing       │ payments │ #payments-oncall │
│ identity      │ platform │ #platform-oncall │
│ postgres      │ platform │ #platform-oncall │
│ storefront    │ web      │ #web-oncall      │
└───────────────┴──────────┴──────────────────┘
4 rows
```

Every service in the registry is in the blast radius of `db1` — a useful
thing for an asset registry to be able to tell you. The
[query cookbook](../working/query-cookbook.md) has many more recipes.

## Version control: a change, on a branch

Now the part that makes acetone acetone. Suppose host `app1` is to be
decommissioned, and its services moved to a new host `app3`. That is a
multi-step change — plan it on a **branch**:

```console
$ acetone branch decommission-app1
created branch "decommission-app1" at 36ecda1611ec97ca85af3a36eb8b9b960dd9b0c1
$ acetone checkout decommission-app1
switched to branch "decommission-app1"
```

On the branch, add the new host and move every service off `app1` — one query
creates the replacement edges and deletes the old ones:

```console
$ acetone query 'CREATE (:Host {name: "app3", region: "eu-west", os: "linux"})'
1 node created
$ acetone query 'MATCH (s:Service)-[r:RUNS_ON]->(:Host {name: "app1"}), (h:Host {name: "app3"}) CREATE (s)-[:RUNS_ON]->(h) DELETE r'
2 relationships created, 2 relationships deleted
$ acetone commit -m "decommission app1: move identity and billing to app3"
committed e79f0e9d15ce8e63e3dd824edaff2754a57a81bd
```

Meanwhile life goes on: back on `main`, an unrelated change lands —

```console
$ acetone checkout main
switched to branch "main"
$ acetone query 'MATCH (s:Service {name: "postgres"}) SET s.version = "16.4"'
1 property set
$ acetone commit -m "postgres upgraded to 16.4"
committed c53d3d3f2eb891f2ba567248c14d2c3fa750f0e5
```

`diff` shows the graph-level difference between any two versions. Note that
it compares the two **endpoints** (not the branch against its fork point), so
main's postgres upgrade also appears, from the branch's point of view, as a
modification:

```console
$ acetone diff main decommission-app1
+ node "Host" ["app3"]
~ node "Service" ["postgres"]
- edge "Service" ["billing"] -"RUNS_ON"-> "Host" ["app1"]
+ edge "Service" ["billing"] -"RUNS_ON"-> "Host" ["app3"]
- edge "Service" ["identity"] -"RUNS_ON"-> "Host" ["app1"]
+ edge "Service" ["identity"] -"RUNS_ON"-> "Host" ["app3"]
```

Both branches have moved since they diverged, so the merge is a true
**three-way merge**: acetone finds the common ancestor and combines both
sides' changes, key by key:

```console
$ acetone merge decommission-app1 -m "merge decommission-app1"
merge commit 86e2684b66cfb885e97ea7be1bf1957c3b89453b
```

The changes did not touch the same properties of the same entities, so the
merge is clean. (When both sides *do* edit the same thing incompatibly, the
merge stops in a resolvable conflict state instead — see
[history, branching and merging](../working/history-branch-merge.md).) The
merged graph has both the new placement and the new postgres version:

```console
$ acetone query 'MATCH (s:Service)-[:RUNS_ON]->(h:Host) RETURN s.name, h.name ORDER BY s.name, h.name'
┌────────────┬────────┐
│ s.name     │ h.name │
├────────────┼────────┤
│ billing    │ app2   │
│ billing    │ app3   │
│ identity   │ app3   │
│ postgres   │ db1    │
│ postgres   │ db2    │
│ storefront │ edge1  │
└────────────┴────────┘
6 rows
$ acetone query 'MATCH (s:Service {name: "postgres"}) RETURN s.version'
┌───────────┐
│ s.version │
├───────────┤
│ 16.4      │
└───────────┘
1 row
```

And the pre-merge world remains a query away — at the seed commit, `app1`
still carried its services:

```console
$ acetone query --at 36ecda1611ec97ca85af3a36eb8b9b960dd9b0c1 'MATCH (s:Service)-[:RUNS_ON]->(h:Host {name: "app1"}) RETURN s.name ORDER BY s.name'
┌──────────┐
│ s.name   │
├──────────┤
│ billing  │
│ identity │
└──────────┘
2 rows
```

One caveat worth knowing at this point: `acetone log` currently follows the
**first-parent** chain only, so the branch's commit does not appear in it
after the merge:

```console
$ acetone log
86e2684b66cfb885e97ea7be1bf1957c3b89453b merge decommission-app1
c53d3d3f2eb891f2ba567248c14d2c3fa750f0e5 postgres upgraded to 16.4
36ecda1611ec97ca85af3a36eb8b9b960dd9b0c1 asset registry: initial inventory
```

Because the repository is plain git underneath, git shows the full picture:

```console
$ git log --oneline --graph --all
*   86e2684 merge decommission-app1
|\
| * e79f0e9 decommission app1: move identity and billing to app3
* | c53d3d3 postgres upgraded to 16.4
|/
* 36ecda1 asset registry: initial inventory
```

## Where the registry goes next

Keep this repository around — later chapters assume it. The
[query cookbook](../working/query-cookbook.md) queries it harder; the
[import chapter](../working/importing.md) feeds it from CSV and shows how
re-imports become clean, provenance-stamped diffs; the
[history chapter](../working/history-branch-merge.md) drives it into a real
merge conflict and out again; and the
[recovery runbook](../recovery/runbook.md) breaks it on purpose. If you ever
want a fresh copy, re-running `asset-registry.sh` in an empty directory
rebuilds it in a few seconds.
