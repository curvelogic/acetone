# Importing data end to end

An asset registry is rarely fed by hand. Inventory arrives as exports from
other systems — a cloud provider's host list, a service catalogue, a
spreadsheet — and lands in the graph through `acetone import`. Import is more
than a loop of `CREATE` statements: it maps rows to nodes or relationships
using the declared schema, records **provenance** (what file, which extractor,
what content hash) in the commit itself, detects when a re-import changes
nothing, and can land its commits on a side branch so a human can review and
merge them. This chapter walks the whole workflow against the
[asset registry](../getting-started/asset-registry.md); every command and
every line of output was produced by running acetone exactly as shown.

The chapter starts from a **fresh copy of the registry** — re-run
[`asset-registry.sh`](../getting-started/asset-registry.sh) in a new empty
directory so your graph holds exactly the seed commit:

```console
$ acetone status
On branch main
HEAD: 8297c9d970393c59de9f005c92b915d369ff03a2
workspace: clean
nodes: 12, edges: 15, schema entries: 7
```

(As ever, your commit hashes will differ; everything else will match.)

## The source files

Four files feed the registry in this chapter (the fourth, `hosts-updated.csv`, arrives later as the drift export). They are available next to
this page — download them to the directory *containing* your repository, the
same place `asset-registry.sh` lives, so the transcripts' `../` paths match.

[`hosts.csv`](hosts.csv) is the infrastructure team's host inventory export.
It carries the five hosts the registry already knows about **plus two new
ones**, `app3` and `db3`:

```csv
{{#include hosts.csv}}
```

[`runs_on.csv`](runs_on.csv) is a placement sheet: which service runs on
which of the new hosts. Each row will become a `RUNS_ON` relationship:

```csv
{{#include runs_on.csv}}
```

[`services.json`](services.json) is the service catalogue — the same four
services the registry knows, but the catalogue also knows each service's
`port`, as a number:

```json
{{#include services.json}}
```

## How import thinks

A few rules shape everything below:

- **One file, one shape.** An import run maps every row to either a node of
  one label (`--label`) or a relationship of one type (`--edge`). A dataset
  with several labels is several files and several runs.
- **The schema drives the mapping.** In node mode, the label's declared key
  picks which fields form the node's identity; the rest become properties.
  The schema (and any relationship type) must be declared *and committed*
  before importing — import refuses a dirty workspace.
- **Import is authoritative.** Each row *replaces* the whole record for its
  key, like `put_node`: the source file is the source of truth for the
  records it carries. Properties the file does not carry are dropped from
  re-imported records — the consequences of that are the subject of the
  [curation section](#import-as-curation-the-mirror-branch) below.
- **Every import that changes the graph is one commit**, stamped with
  provenance trailers. An import that changes nothing writes no commit.

Supported formats: `csv` (header row; every cell is a **string**), `json`
(an array of flat objects, values keeping their JSON types) and `ndjson`
(one flat object per line). Nested JSON objects and nested lists are
rejected — the v0.1 data model is flat property bags of scalars and lists
of scalars.

## Nodes from CSV

From inside the repository, import the host inventory:

```console
$ acetone import --format csv ../hosts.csv --label Host
imported 7 node(s) and 0 edge(s) onto the current branch; commit e5e2b918a7ef20cab50df2df5d3b29f06c5578b7
```

`Host`'s declared key is `name`, so the `name` column became each node's
identity and `region` and `os` became properties. The import is a commit —
with a synthesised message (pass `-m` to supply your own) and the three
provenance trailers:

```console
$ acetone log
e5e2b918a7ef20cab50df2df5d3b29f06c5578b7 Import 7 node(s) and 0 edge(s) from ../hosts.csv via csv
    Acetone-Source: ../hosts.csv
    Acetone-Extractor: csv
    Acetone-Source-Hash: f961337fe6981739e07185c4d11473688ca4e72df0126105cff5cf0aebe9afb2
8297c9d970393c59de9f005c92b915d369ff03a2 asset registry: initial inventory
```

`Acetone-Source-Hash` is the SHA-256 of the raw source bytes — months later
you can prove exactly which file produced which commit.

Note the count: **7 nodes imported**, because the file has seven rows and
import processes them all. But five of those rows carried exactly the values
the graph already had, so replacing those records changed nothing. `diff`
tells the truth about what actually changed:

```console
$ acetone diff 8297c9d970393c59de9f005c92b915d369ff03a2 main
+ node "Host" ["app3"]
+ node "Host" ["db3"]
```

The imported count is rows processed; the diff is the graph-level change.
And the graph now has all seven hosts:

```console
$ acetone query 'MATCH (h:Host) RETURN h.name, h.region, h.os ORDER BY h.name'
┌────────┬────────────┬─────────┐
│ h.name │ h.region   │ h.os    │
├────────┼────────────┼─────────┤
│ app1   │ eu-west    │ linux   │
│ app2   │ eu-central │ linux   │
│ app3   │ eu-west    │ linux   │
│ db1    │ eu-west    │ linux   │
│ db2    │ eu-central │ linux   │
│ db3    │ eu-central │ linux   │
│ edge1  │ eu-west    │ freebsd │
└────────┴────────────┴─────────┘
7 rows
```

One thing to know about CSV: it has no types, so **every imported cell is a
string**. That is fine here — `region` and `os` are strings anyway — but a
numeric column imported from CSV is a string property, not a number. Use
JSON when types matter (below).

## Re-importing an unchanged source is a no-op

Run the same import again:

```console
$ acetone import --format csv ../hosts.csv --label Host
import produced no graph changes; nothing to commit
```

No commit was written — `log` and `status` are exactly as before. The check
is on the *graph*, not the file: import applies all the rows and then notices
the workspace still matches HEAD, so there is nothing to commit. A scheduled
nightly import therefore leaves no trail of empty commits on the nights when
nothing changed; commits appear exactly when the data moved.

## Relationships from CSV

Edge mode maps each row to one relationship. You name the relationship type
with `--edge`, and tell import which columns identify the two endpoints with
`--from` and `--to`, each as `LABEL=field` (comma-separate several fields
when the endpoint label has a composite key, in key order):

```console
$ acetone import --format csv ../runs_on.csv --edge RUNS_ON --from Service=service --to Host=host -m "placement: the new hosts take billing and postgres"
imported 0 node(s) and 2 edge(s) onto the current branch; commit 417d787c8d1323341be4ea81e98ec9071a730d83
```

The `service` and `host` columns were consumed as endpoint keys; had the file
carried any further columns, they would have become edge properties. The
placements are live:

```console
$ acetone query 'MATCH (s:Service)-[:RUNS_ON]->(h:Host) WHERE h.name IN ["app3", "db3"] RETURN s.name, h.name'
┌──────────┬────────┐
│ s.name   │ h.name │
├──────────┼────────┤
│ billing  │ app3   │
│ postgres │ db3    │
└──────────┴────────┘
2 rows
```

Order matters: **import nodes before the edges that reference them**. Edge
import checks that both endpoints exist and refuses to create a dangling
relationship (you will see the error in a moment).

## Typed values from JSON

CSV flattens everything to strings; JSON keeps its types. The service
catalogue carries each `port` as a number:

```console
$ acetone import --format json ../services.json --label Service
imported 4 node(s) and 0 edge(s) onto the current branch; commit c61180f80736bdb1abacdbd61dafa9356d41498a
```

The ports arrived as integers — a numeric comparison works directly:

```console
$ acetone query 'MATCH (s:Service) WHERE s.port < 1024 RETURN s.name, s.port'
┌────────────┬────────┐
│ s.name     │ s.port │
├────────────┼────────┤
│ storefront │ 443    │
└────────────┴────────┘
1 row
```

Notice what the file had to carry. Import replaces whole records, so
`services.json` includes `tier` and `version` alongside the new `port` —
had it carried only `name` and `port`, the import would have *removed*
`tier` and `version` from every service. A source file must carry every
property it is authoritative for:

```console
$ acetone query 'MATCH (s:Service) RETURN s.name, s.tier, s.version, s.port ORDER BY s.name'
┌────────────┬────────┬───────────┬────────┐
│ s.name     │ s.tier │ s.version │ s.port │
├────────────┼────────┼───────────┼────────┤
│ billing    │ core   │ 7.0.2     │ 8080   │
│ identity   │ core   │ 2.4.1     │ 7000   │
│ postgres   │ data   │ 16.3      │ 5432   │
│ storefront │ edge   │ 2026.28   │ 443    │
└────────────┴────────┴───────────┴────────┘
4 rows
```

(`--format ndjson` works identically, reading one object per line — the
natural fit for log-shaped exports.)

## When the source is wrong

Import fails *before* touching the graph: the file is parsed and mapped
first, and any error leaves the repository exactly as it was — no partial
import, no dirty workspace, no commit. What the failures look like, for
real:

**An undeclared label.** Suppose a `racks.csv` arrives (`name,site` /
`r1,dc-lux`) before anyone has declared `Rack`:

```console
$ acetone import --format csv ../racks.csv --label Rack
error: importing: import mapping: no schema for label "Rack"; declare it before importing
```

**A missing key column.** This `hosts-nokey.csv` calls its first column
`hostname`, but `Host`'s declared key property is `name`:

```csv
hostname,region,os
edge2,eu-central,freebsd
```

```console
$ acetone import --format csv ../hosts-nokey.csv --label Host
error: importing: import mapping: record for "Host" is missing key property "name"
```

(Import maps columns to properties by name, and there is no renaming flag —
fix the header, or export the file with the right column names.)

**A wrong endpoint column in an edge file.** Here the file says `svc` where
the command says `--from Service=service`:

```console
$ acetone import --format csv ../runs_on-badcol.csv --edge RUNS_ON --from Service=service --to Host=host
error: importing: import mapping: edge row is missing endpoint key field "service" for label "Service"
```

**An edge to a node that does not exist.** A placement row naming a host
`ghost9` is refused — import will not create a dangling relationship:

```console
$ acetone import --format csv ../runs_on-ghost.csv --edge RUNS_ON --from Service=service --to Host=host
error: importing: operation would leave a dangling RUNS_ON relationship: its target endpoint node "Host" ["ghost9"] does not exist
```

**A dirty workspace.** Import refuses to run on top of uncommitted edits, so
an import commit is never polluted with unrelated staged changes and no-op
detection stays trustworthy:

```console
$ acetone query 'MATCH (h:Host {name: "edge1"}) SET h.os = "openbsd"'
1 property set
$ acetone import --format csv ../hosts.csv --label Host
error: importing: workspace has uncommitted changes; commit them first
```

Commit the edit or undo it, then re-run. (Here we undo it — setting the
property back to its committed value, after which `status` reports the
workspace clean again, because dirtiness is judged on content, not on
having issued commands.)

**A row violating a declared constraint.** The registry's schema declares
`--require tier` on `Service`, and import enforces it exactly as Cypher
`CREATE` does. A catalogue row with no `tier` fails the **whole import** —
nothing is committed, the workspace stays clean:

```console
$ acetone import --format json ../services-notier.json --label Service
error: importing: import violates declared constraints — 1 constraint violation:
  node "Service" ["search"] is missing required property "tier"
```

`--unique` constraints are enforced the same way, both against data already
in the graph and between rows of the same file. Every violation is listed
(the first twenty, plus a count of the rest), so one failed run names
everything to fix in the source. Fix the source and re-run — import is
all-or-nothing, so there is never a half-landed file to clean up.

One honest caveat: a repository written **before** acetone enforced
constraints on import (v0.3.0 and earlier) may already hold violating
records. Import does not punish the messenger — a new import fails only for
violations its own rows are involved in — and `acetone fsck` names any
pre-existing breaches as advisories, so they surface without turning the
repository radioactive.

There is **no `--dry-run` flag**. The honest equivalent is `--branch`: import
onto a scratch branch, inspect it with `diff` and `query --at`, and merge it
only if you like what you see — that workflow is the next section. (A branch
you decide against is deleted with plain `git branch -D <name>`; branch
management is one of the areas where git and acetone interoperate freely.)

## Import as curation: the mirror branch

Now the workflow that makes import and version control more than the sum of
their parts. The tension: import is **authoritative-replace**, but humans
curate the registry between imports — an owner note here, a risk annotation
there — properties the source system knows nothing about. If the monthly
inventory export is imported straight onto `main`, every curated property on
an imported record is silently dropped, because the source file does not
carry it.

The answer (ADR-0042) is to keep a **one-directional mirror branch**: the
importer only ever commits to it, and humans only ever merge *from* it.
acetone's three-way merge is cell-wise — per property, not per record — so a
curated property the source does not carry is a one-sided change that merges
cleanly, forever.

Set up the mirror branch by pointing the first `--branch` import at it.
`--branch` creates the branch if it is absent, lands the import there in
isolation, and leaves your current branch checked out and untouched:

```console
$ acetone import --format csv ../hosts.csv --label Host --branch ingest
import produced no graph changes; nothing to commit
$ acetone branch
  ingest
* main
```

This first run is a no-op (the graph already matches `hosts.csv`) but the
branch now exists. **Start the mirror before curation begins**: the branch
must fork from a point whose records are pure source data, so that curated
properties never appear in a future merge base.

Now curate. The team decides `app3` is the canary box, and records it —
on `main`, where humans work:

```console
$ acetone query 'MATCH (h:Host {name: "app3"}) SET h.note = "canary: new capacity, watch error rates"'
1 property set
$ acetone commit -m "annotate app3 as the canary host"
committed 7b88e3f8c31eb52610dc75f9dd88a8be99bcfc21
```

A month passes. The next inventory export,
[`hosts-updated.csv`](hosts-updated.csv), reports that `app2` was rebuilt in
`eu-west` and a new host `edge2` exists. The importer lands it on the
mirror:

```console
$ acetone import --format csv ../hosts-updated.csv --label Host --branch ingest
imported 8 node(s) and 0 edge(s) onto ingest; commit 52b72fbcb8402b62faba999abb933f8ee47fb42e
$ acetone status
On branch main
HEAD: 7b88e3f8c31eb52610dc75f9dd88a8be99bcfc21
workspace: clean
nodes: 14, edges: 17, schema entries: 7
```

`main` has not moved. And look at `app3` *on the mirror* — the import
replaced its record with the file's columns, so the note is not there:

```console
$ acetone query --at ingest 'MATCH (h:Host {name: "app3"}) RETURN h.note'
┌────────┐
│ h.note │
├────────┤
│ NULL   │
└────────┘
1 row
```

That is exactly what a direct import onto `main` would have made of your
curated record. On the mirror it is harmless — the merge is about to prove
it. First, review what the source is proposing. Remember from
[the previous chapter](../getting-started/asset-registry.md) that `diff`
compares endpoints, so the missing note also shows up here, as a
modification of `app3` from the mirror's point of view:

```console
$ acetone diff main ingest
~ node "Host" ["app2"]
~ node "Host" ["app3"]
+ node "Host" ["edge2"]
```

Merge on your own cadence:

```console
$ acetone merge ingest -m "take the July host inventory"
merge commit b8c8c9824230a735820fc96d12ce49c04f8e30cd
```

The merge base is the mirror's fork point, where `app3` had no note. The
note is therefore a change on `main`'s side only, and the region change and
new host are changes on the mirror's side only — cell-wise merge combines
them without conflict. The curation survived the re-import, and the source's
updates landed:

```console
$ acetone query 'MATCH (h:Host {name: "app3"}) RETURN h.region, h.note'
┌──────────┬─────────────────────────────────────────┐
│ h.region │ h.note                                  │
├──────────┼─────────────────────────────────────────┤
│ eu-west  │ canary: new capacity, watch error rates │
└──────────┴─────────────────────────────────────────┘
1 row
$ acetone query 'MATCH (h:Host) RETURN h.name, h.region ORDER BY h.name'
┌────────┬────────────┐
│ h.name │ h.region   │
├────────┼────────────┤
│ app1   │ eu-west    │
│ app2   │ eu-west    │
│ app3   │ eu-west    │
│ db1    │ eu-west    │
│ db2    │ eu-central │
│ db3    │ eu-central │
│ edge1  │ eu-west    │
│ edge2  │ eu-central │
└────────┴────────────┘
8 rows
```

Had a human and the source changed the *same* property of the same node, the
merge would instead have stopped in a conflict for the human to resolve —
conflicts are data, not errors, and the
[history chapter](history-branch-merge.md) walks one end to end.

> **The one-directional rule.** This works *because* curation never reaches
> the mirror: the importer only commits source records to `ingest`, and you
> only merge `ingest → main`. Never merge `main → ingest`, and never import
> the source directly onto `main`. If a curated property ever gets into the
> mirror's history it enters the next merge base, and the following
> re-import — which drops the property, because the source does not carry
> it — reads as a clean deletion and erases the curation *silently, with no
> conflict*. The discipline is a property of the workflow, not enforced by
> the store.

## Round trips: export

`export` is import's inverse: it writes a graph version out as one table per
label and per relationship type, in the same three formats. A single label
goes to stdout by default — note the curated `note` column riding along:

```console
$ acetone export --format csv --label Host
name,note,os,region
app1,,linux,eu-west
app2,,linux,eu-west
app3,"canary: new capacity, watch error rates",linux,eu-west
db1,,linux,eu-west
db2,,linux,eu-central
db3,,linux,eu-central
edge1,,freebsd,eu-west
edge2,,freebsd,eu-central
exported 8 Host node(s)
```

With no `--label` or `--edge`, `-o` names a directory and the whole graph is
written, one file per table (edge tables carry `src` and `dst` key columns):

```console
$ acetone export --format json -o ../tables
exported 8 node(s) → ../tables/Host.json
exported 4 node(s) → ../tables/Service.json
exported 3 node(s) → ../tables/Team.json
exported 5 edge(s) → ../tables/rel-DEPENDS_ON.json
exported 4 edge(s) → ../tables/rel-OWNS.json
exported 8 edge(s) → ../tables/rel-RUNS_ON.json
```

**JSON round-trips faithfully.** Export a label and import the file straight
back, and import recognises there is nothing to do:

```console
$ acetone export --format json --label Service -o ../service-export.json
exported 4 Service node(s)
$ acetone import --format json ../service-export.json --label Service
import produced no graph changes; nothing to commit
```

**CSV does not**, because CSV has no types: the services' integer ports
export as text and come back as strings, which is a different value. Probe
it safely on a scratch branch:

```console
$ acetone export --format csv --label Service -o ../service-export.csv
exported 4 Service node(s)
$ acetone import --format csv ../service-export.csv --label Service --branch scratch
imported 4 node(s) and 0 edge(s) onto scratch; commit 8550e395afea4a0266227c276444655bb4d05720
$ acetone diff main scratch
~ node "Service" ["billing"]
~ node "Service" ["identity"]
~ node "Service" ["postgres"]
~ node "Service" ["storefront"]
$ git branch -D scratch
Deleted branch scratch (was 8550e39).
```

Every service shows as modified — that is `port: 5432` becoming
`port: "5432"`. The rule of thumb: **CSV for feeding spreadsheets and
humans, JSON (or NDJSON) for round trips and backups.**

A full JSON export is enough to rebuild the graph's current state from
nothing: initialise a fresh repository, declare the same schema, import the
node tables and then the edge tables (nodes first — dangling edges are
refused):

```console
$ cd .. && mkdir rebuild && cd rebuild
$ acetone init
Initialized empty acetone repository in .
$ acetone declare-label Team --key name
declared label "Team" key ["name"]
...remaining schema declarations as in asset-registry.sh...
$ acetone commit -m "schema"
committed 11e18c8ca4b501c4e935fa11abef22f5b965b3bc
$ acetone import --format json ../tables/Team.json --label Team
imported 3 node(s) and 0 edge(s) onto the current branch; commit 6a205f02ae3e87ec6462483d26a4b5e00f59ab4d
$ acetone import --format json ../tables/Host.json --label Host
imported 8 node(s) and 0 edge(s) onto the current branch; commit 2eff3daf900246b3d415293f1fb0b56e9eaebe22
$ acetone import --format json ../tables/Service.json --label Service
imported 4 node(s) and 0 edge(s) onto the current branch; commit a49844045ba753b085da065ed03fb24b975837a4
$ acetone import --format json ../tables/rel-OWNS.json --edge OWNS --from Team=src --to Service=dst
imported 0 node(s) and 4 edge(s) onto the current branch; commit fc850ddabb7a5c07d24bc9799b4b99ecf66812ef
$ acetone import --format json ../tables/rel-RUNS_ON.json --edge RUNS_ON --from Service=src --to Host=dst
imported 0 node(s) and 8 edge(s) onto the current branch; commit 0a42ff5bd91f04a8426a7723f5c82c6eae9bbfab
$ acetone import --format json ../tables/rel-DEPENDS_ON.json --edge DEPENDS_ON --from Service=src --to Service=dst
imported 0 node(s) and 5 edge(s) onto the current branch; commit affcc45fef49177eaff3d074e686784c7553be30
$ acetone status
On branch main
HEAD: affcc45fef49177eaff3d074e686784c7553be30
workspace: clean
nodes: 15, edges: 17, schema entries: 7
$ acetone query 'MATCH (h:Host {name: "app3"}) RETURN h.note'
┌─────────────────────────────────────────┐
│ h.note                                  │
├─────────────────────────────────────────┤
│ canary: new capacity, watch error rates │
└─────────────────────────────────────────┘
1 row
```

Fifteen nodes, seventeen edges, curation intact — the same *state* as the
original, though not the same *history*: history travels as git commits
(`git clone`, `git push`), while export travels as tables. Use git when you
want the past to come along; use export when you want the present in a shape
other tools can read.

That is the full loop: sources feed the mirror branch, humans review, merge
and curate, and export hands the graph back to the rest of the world. The
[next chapter](history-branch-merge.md) looks harder at what history itself
can do — including what happens when a merge does not go cleanly.
