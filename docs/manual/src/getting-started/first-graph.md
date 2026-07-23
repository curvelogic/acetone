# Your first graph

This chapter walks the minimal happy path end to end: create a repository,
declare a schema, create and query some data, commit it, change it, and look
back through history. Every command and every line of output below was
produced by running acetone exactly as shown; replay it yourself and
everything matches, except the commit hashes, which differ from repository
to repository.

## Create a repository

Make an empty directory and initialise it:

```console
$ mkdir hello && cd hello
$ acetone init
Initialized empty acetone repository in .
```

Have a look at what appeared:

```console
$ ls
config  description  HEAD  hooks  info  objects  refs
```

That is a **bare git repository**. There is no working tree of files to edit —
the graph lives in the object store, and acetone commands are how you read and
write it. (You can point `git log` at it, as we will below, but all writes go
through acetone.)

## Declare a schema

In acetone, a node's identity is its **primary label plus a natural key** —
a tuple of one or more property values you declare per label. There are no
auto-generated internal IDs to leak into your data: `("Host", ["web1"])` *is*
the node's identity, in every version, on every branch. This is what makes
history diffable and mergeable — the same real-world thing has the same
identity everywhere, so two branches editing "the host called web1" are
editing the same node.

The consequence: before Cypher can persist a node, its label's key must be
declared. Trying to create a node with an undeclared label fails cleanly:

```console
$ acetone query 'CREATE (:Host {name: "web1"})'
error: cannot persist node: none of the labels ["Host"] declares a key, so this node has no identity (Invariant #3) — declare one first, e.g. `acetone declare-label "Host" --key <property>`
```

(Once at least one label exists, referencing an undeclared label fails
earlier, when the query is bound — and a near-miss of a declared label gets a
suggestion: `error: line 1, column 8: unknown label "Hots" (not declared in
the schema) — did you mean "Host"?`.)

So declare the schema first — two labels, keyed by `name`, and one
relationship type:

```console
$ acetone declare-label Host --key name
declared label "Host" key ["name"]
$ acetone declare-label Service --key name
declared label "Service" key ["name"]
$ acetone declare-rel-type DEPENDS_ON
declared relationship type "DEPENDS_ON"
```

(`--key` can be repeated for a composite key; the order matters.)

## Create some data

Now Cypher `CREATE` works:

```console
$ acetone query 'CREATE (:Host {name: "web1", os: "linux"})'
1 node created
$ acetone query 'CREATE (:Host {name: "web2", os: "linux"}), (:Service {name: "billing", port: 8080})'
2 nodes created
```

Connect the service to the host it depends on — match both endpoints, then
create the relationship:

```console
$ acetone query 'MATCH (s:Service {name: "billing"}), (h:Host {name: "web1"}) CREATE (s)-[:DEPENDS_ON]->(h)'
1 relationship created
```

## Query it back

```console
$ acetone query 'MATCH (h:Host) RETURN h.name, h.os ORDER BY h.name'
┌────────┬───────┐
│ h.name │ h.os  │
├────────┼───────┤
│ web1   │ linux │
│ web2   │ linux │
└────────┴───────┘
2 rows
$ acetone query 'MATCH (s:Service)-[:DEPENDS_ON]->(h:Host) RETURN s.name, h.name'
┌─────────┬────────┐
│ s.name  │ h.name │
├─────────┼────────┤
│ billing │ web1   │
└─────────┴────────┘
1 row
```

`--format json` or `--format csv` reshape the output for scripts; for
interactive exploration, `acetone shell` starts a readline Cypher REPL.

## Commit

Everything so far — the three schema declarations, three nodes and one edge —
is sitting in the **workspace**: real, queryable, but not yet history.
`status` shows the workspace is dirty; `commit` turns it into a commit:

```console
$ acetone status
On branch main
HEAD: (no commits yet)
workspace: dirty
nodes: 3, edges: 1, schema entries: 3
$ acetone commit -m "declare schema; seed two hosts and the billing service"
committed f0cd20bdf7c2c6e7f668355d4c371274620c0235
$ acetone status
On branch main
HEAD: f0cd20bdf7c2c6e7f668355d4c371274620c0235
workspace: clean
nodes: 3, edges: 1, schema entries: 3
```

Note there is no staging step: a commit snapshots the whole workspace —
schema and data together. The schema is versioned exactly like the data.

```console
$ acetone log
f0cd20bdf7c2c6e7f668355d4c371274620c0235 declare schema; seed two hosts and the billing service
$ acetone schema
Labels
  "Host"     key ("name")
  "Service"  key ("name")
Relationship types
  "DEPENDS_ON"
Indexes
  (none)
```

## Change something, and look back

Make a change with `SET` and commit again:

```console
$ acetone query 'MATCH (h:Host {name: "web2"}) SET h.os = "freebsd"'
1 property set
$ acetone commit -m "web2 reinstalled with FreeBSD"
committed 02560fb2db8d94dc283e15098a3d8dd54bbab93d
$ acetone log
02560fb2db8d94dc283e15098a3d8dd54bbab93d web2 reinstalled with FreeBSD
f0cd20bdf7c2c6e7f668355d4c371274620c0235 declare schema; seed two hosts and the billing service
```

Now the git-like part pays off. `query --at <ref>` runs any read against any
point in history — **time travel**:

```console
$ acetone query --at f0cd20bdf7c2c6e7f668355d4c371274620c0235 'MATCH (h:Host {name: "web2"}) RETURN h.os'
┌───────┐
│ h.os  │
├───────┤
│ linux │
└───────┘
1 row
```

And `diff` shows what changed between two versions, at the graph level —
nodes and relationships added (`+`), removed (`-`) or modified (`~`),
identified by label and key:

```console
$ acetone diff f0cd20bdf7c2c6e7f668355d4c371274620c0235 main
~ node "Host" ["web2"]
```

## It really is git underneath

The commits you just made are git commits. Plain git works on the same
directory:

```console
$ git log --oneline
02560fb web2 reinstalled with FreeBSD
f0cd20b declare schema; seed two hosts and the billing service
```

Backup and transport are therefore just git: add a remote and `git push`, or
`git clone` the repository somewhere else — the remote never needs to know
acetone exists. The rule of thumb from the [installing chapter](installing.md)
bears repeating: **read with either, write only with acetone**.

That is the whole core loop: `init` → declare schema → mutate with Cypher →
`commit` → repeat, with `log`, `--at` and `diff` to move around history. The
[next chapter](asset-registry.md) builds the running example the rest of this
manual uses — and adds branching and merging to the loop.
