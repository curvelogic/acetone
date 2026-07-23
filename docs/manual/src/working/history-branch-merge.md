# History, branching and merging

Every acetone version is a git commit, every branch is a git ref, and a merge
is a real three-way merge computed over the graph itself. This chapter drives
the [asset registry](../getting-started/asset-registry.md) through the full
version-control workflow: reading history, working with branches and the
workspace, diffing versions, and — the centrepiece — a merge that genuinely
conflicts, and what you do about it.

It continues from where Part I left the registry: `app1` decommissioned on a
branch and merged back, postgres at 16.4. (The outputs below come from a fresh
replay of that flow, so the commit hashes differ from Part I's — as yours
will. Everything else matches.)

## Reading history

`acetone log` lists commits from the current branch's head, newest first:

```console
$ acetone log
d23309799d0583dc54b709db2a507a2736426acb merge decommission-app1
3e59a55c23a9c2d8be568974106ee1d412026ffb postgres upgraded to 16.4
4e04e05af4c92472342ade7ba874e9a2994d44cb asset registry: initial inventory
```

One nuance, worth restating from Part I because you will notice it every
time you merge: plain `acetone log` follows the **first-parent** chain — the
current branch's own changelog — so the commit made on the
`decommission-app1` branch, although an ancestor of the merge commit, does
not appear in the list above. `acetone log --all` covers the whole commit
graph instead: every commit reachable from any branch, each exactly once,
newest first, with both parent hashes shown on merge commits:

```console
$ acetone log --all
d23309799d0583dc54b709db2a507a2736426acb merge decommission-app1
merge: 3e59a55c23a9c2d8be568974106ee1d412026ffb 19f7e936e8edb5ae247f04cdc026a3af9eefea7e
3e59a55c23a9c2d8be568974106ee1d412026ffb postgres upgraded to 16.4
19f7e936e8edb5ae247f04cdc026a3af9eefea7e decommission app1: move identity and billing to app3
4e04e05af4c92472342ade7ba874e9a2994d44cb asset registry: initial inventory
```

The `merge:` line reads `[ours, theirs]` — the first parent is the branch
that was checked out, the second the branch merged in. (It sits flush at
the start of the line, unlike a commit's trailers, which always render
indented — so a trailer that happens to be *named* `merge` cannot pass
itself off as merge structure.) And since the repository is plain git
underneath, git itself can draw the topology:

```console
$ git log --oneline --graph --all
*   d233097 merge decommission-app1
|\
| * 19f7e93 decommission app1: move identity and billing to app3
* | 3e59a55 postgres upgraded to 16.4
|/
* 4e04e05 asset registry: initial inventory
```

History is also queryable from inside Cypher, where it lands as ordinary rows
you can filter and join:

```console
$ acetone query 'CALL acetone.log()'
┌──────────────────────────────────────────┬───────────────────────────────────┐
│ commit                                   │ subject                           │
├──────────────────────────────────────────┼───────────────────────────────────┤
│ d23309799d0583dc54b709db2a507a2736426acb │ merge decommission-app1           │
│ 3e59a55c23a9c2d8be568974106ee1d412026ffb │ postgres upgraded to 16.4         │
│ 4e04e05af4c92472342ade7ba874e9a2994d44cb │ asset registry: initial inventory │
└──────────────────────────────────────────┴───────────────────────────────────┘
3 rows
```

## The workspace, and when you may switch branches

Writes do not go straight into commits. They accumulate in the **workspace** —
Dolt-style working state that survives process exit — until `acetone commit`
turns it into a git commit. `acetone status` tells you which state you are in:

```console
$ acetone query 'MATCH (s:Service {name: "identity"}) SET s.version = "2.4.2"'
1 property set
$ acetone status
On branch main
HEAD: d23309799d0583dc54b709db2a507a2736426acb
workspace: dirty
nodes: 13, edges: 15, schema entries: 7
```

A dirty workspace pins you to the current branch. `checkout` refuses, because
switching would abandon the uncommitted changes:

```console
$ acetone checkout decommission-app1
error: checking out branch "decommission-app1": workspace has uncommitted changes; commit them first
```

Creating a branch is fine, though — it only plants a ref at the current head
commit and touches nothing in the workspace:

```console
$ acetone branch bump-identity
created branch "bump-identity" at d23309799d0583dc54b709db2a507a2736426acb
```

Two things to know about this model:

- **Merging also requires a clean workspace** — you will see the same
  "uncommitted changes" refusal from `acetone merge`.
- **There is currently no discard command.** If you change your mind about an
  uncommitted change, write the inverse (`SET` the old value back, `DELETE`
  what you created) or commit it and move on. `checkout` refuses even a
  checkout of the branch you are already on, so it cannot be used as a reset.

Commit the version bump to get back to a clean state:

```console
$ acetone commit -m "identity patch release 2.4.2"
committed f0f5b30d664e6dfabbaf61f9c8efa315e9681de6
```

## Manufacturing a disagreement

To see conflict handling for real, we need two branches that disagree. The
`bump-identity` branch was created above, planted at the pre-2.4.2 merge
commit. Now let the two lines of history diverge.

On `main`, operations decides `db2` is to be retired, and takes it out of the
registry along with everything attached to it:

```console
$ acetone query 'MATCH (h:Host {name: "db2"}) DETACH DELETE h'
1 node deleted, 1 relationship deleted
$ acetone commit -m "retire db2"
committed bce960e3498f2360d5e808b31288eb0ae7f245e0
```

Meanwhile on `bump-identity`, the identity team ships 2.5.0 and — not knowing
about the retirement — scales the service out onto `db2`:

```console
$ acetone checkout bump-identity
switched to branch "bump-identity"
$ acetone query 'MATCH (s:Service {name: "identity"}) SET s.version = "2.5.0"'
1 property set
$ acetone query 'MATCH (s:Service {name: "identity"}), (h:Host {name: "db2"}) CREATE (s)-[:RUNS_ON]->(h)'
1 relationship created
$ acetone commit -m "identity 2.5.0: scale out onto db2"
committed c67b3adef6b307a471a52fe4d45be43e6b75f21d
```

The two branches now disagree twice over: `main` says identity is 2.4.2 and
`db2` does not exist; `bump-identity` says identity is 2.5.0 and runs on
`db2`.

## Diffing between versions

`acetone diff` compares any two versions — branch short names, full ref names
or commit hashes. Note that it compares the two **endpoints**, not the branch
against its fork point, so changes made on `main` since the branches diverged
also show up, read from `main`'s side:

```console
$ acetone checkout main
switched to branch "main"
$ acetone diff main bump-identity
+ node "Host" ["db2"]
~ node "Service" ["identity"]
+ edge "Service" ["identity"] -"RUNS_ON"-> "Host" ["db2"]
+ edge "Service" ["postgres"] -"RUNS_ON"-> "Host" ["db2"]
```

From `main`'s point of view, `bump-identity` "adds" `db2` and its postgres
placement (because `main` deleted them), modifies identity (both sides did),
and adds the new `RUNS_ON` edge.

## A merge that conflicts

Merge the branch. acetone finds the common ancestor through the git commit
graph and three-way-merges each of the graph's maps, key by key. Where the
two sides made compatible changes they are simply combined — but identity's
`version` was edited to different values on both sides, and no automatic
answer exists:

```console
$ acetone merge bump-identity -m "merge bump-identity"
merge produced 1 conflict(s):
  node "Service" ["identity"] property "version"
resolve with `acetone resolve --all-ours|--all-theirs` (or write the conflicted entities), then `acetone commit` to complete — or `acetone merge --abort` to back out
error: merge conflicts remain
```

The command exits non-zero, but nothing has been damaged and nothing needs
un-doing. **Conflicts are data, not errors** (spec §6): the workspace has
entered a *merge-in-progress* state in which the non-conflicting 95% of the
merge is already applied, and the conflicts sit in a queryable `conflicts`
map awaiting your decision. `status` shows the state:

```console
$ acetone status
On branch main
HEAD: bce960e3498f2360d5e808b31288eb0ae7f245e0
workspace: dirty
merge: in progress, 1 conflict(s) to resolve (`acetone resolve --all-ours|--all-theirs`, or write the conflicted entities directly), or `acetone merge --abort`
nodes: 12, edges: 15, schema entries: 7
```

And because conflicts are data, you inspect them with a query. Each row opens
with a `kind` column classifying the conflict — `cell` here, the graph-level
violation classes later in this chapter — and carries the property in
question and all three values: the common ancestor (`base`), the current
branch (`ours`) and the merged-in branch (`theirs`), plus the merged node
itself as a `_Conflict`-labelled virtual element:

```console
$ acetone query 'CALL acetone.conflicts()'
┌──────┬─────────┬────────────────────────┬──────────┬───────┬───────┬────────┬───────────────────────────────────────────────────────────────────┐
│ kind │ label   │ key                    │ property │ base  │ ours  │ theirs │ node                                                              │
├──────┼─────────┼────────────────────────┼──────────┼───────┼───────┼────────┼───────────────────────────────────────────────────────────────────┤
│ cell │ Service │ "Service" ["identity"] │ version  │ 2.4.1 │ 2.4.2 │ 2.5.0  │ (:_Conflict:Service {name: identity, tier: core, version: 2.4.2}) │
└──────┴─────────┴────────────────────────┴──────────┴───────┴───────┴────────┴───────────────────────────────────────────────────────────────────┘
1 row
```

## Backing out: `merge --abort`

If now is not the moment, abort. The workspace is restored to the branch tip
as if the merge had never been attempted:

```console
$ acetone merge --abort
merge aborted — workspace restored to the branch tip
$ acetone status
On branch main
HEAD: bce960e3498f2360d5e808b31288eb0ae7f245e0
workspace: clean
nodes: 12, edges: 14, schema entries: 7
```

Merging is a pure function of `(base, ours, theirs)` — determinism here is
normative, per
[the specification](https://github.com/curvelogic/acetone/blob/main/docs/acetone-02-spec.md)'s
diff-and-merge section — so re-running the merge later
reproduces exactly the same result, conflict included:

```console
$ acetone merge bump-identity -m "merge bump-identity"
merge produced 1 conflict(s):
  node "Service" ["identity"] property "version"
resolve with `acetone resolve --all-ours|--all-theirs` (or write the conflicted entities), then `acetone commit` to complete — or `acetone merge --abort` to back out
error: merge conflicts remain
```

## Resolving

Cell conflicts are resolved either by **ordinary writes** (just `SET` the
value you want on the conflicted entity) or wholesale with `acetone resolve`,
taking every conflicted value from one side. Identity 2.5.0 genuinely
shipped, so take the incoming side:

```console
$ acetone resolve --all-theirs
resolved 1 conflict(s), but the resolved graph has 1 graph-level violation(s):
  dangling relationship "Service" ["identity"] -"RUNS_ON"-> "Host" ["db2"]: destination node "Host" ["db2"] is absent
repair the graph (delete the dangling relationship, restore the endpoint, or fix the constraint breach), then `acetone commit` to complete — or `acetone merge --abort` to back out
```

The cell conflict is settled — and resolving it has exposed the **second
kind of conflict**: a **graph-level violation**. Each side was internally
consistent — `main` deleted `db2` and every edge touching it; `bump-identity`
added an edge to a `db2` that existed — but their combination contains an
edge whose target node no longer exists. While the `version` conflict was
outstanding the merged graph was incomplete, so acetone could not judge it;
now that every cell conflict is resolved it re-validates the whole graph, and
reports the breach the same way it reports everything else — as data. The
`kind` column names the violation class, and `property` says which endpoint
of the relationship is absent:

```console
$ acetone query 'CALL acetone.conflicts()'
┌───────────────┬─────────┬────────────────────────────────────────────────────┬──────────┬──────┬──────┬────────┬──────┐
│ kind          │ label   │ key                                                │ property │ base │ ours │ theirs │ node │
├───────────────┼─────────┼────────────────────────────────────────────────────┼──────────┼──────┼──────┼────────┼──────┤
│ dangling-edge │ RUNS_ON │ "Service" ["identity"] -"RUNS_ON"-> "Host" ["db2"] │ dst      │ NULL │ NULL │ NULL   │ NULL │
└───────────────┴─────────┴────────────────────────────────────────────────────┴──────────┴──────┴──────┴────────┴──────┘
1 row
```

There is no side to pick for a violation, so `resolve` does not apply;
completion is gated on it instead. `acetone commit` **re-validates the merged
graph** before writing the two-parent commit, and refuses while any violation
remains — naming each one:

```console
$ acetone commit -m "merge bump-identity"
error: committing workspace: cannot commit: the merge leaves 1 graph-level violation(s) — repair the graph (delete the dangling relationship, restore the endpoint, or fix the constraint breach), then commit: dangling relationship "Service" ["identity"] -"RUNS_ON"-> "Host" ["db2"]: destination node "Host" ["db2"] is absent
```

A dangling edge is invisible to `MATCH` (patterns only traverse edges with
both endpoints), so the conflict report and the commit refusal above are how
you find out about one mid-merge.
[`acetone fsck`](maintenance-and-migration.md) confirms the same breach
independently, checking the workspace along with committed history:

```console
$ acetone fsck
[error] workspace refs/worktree/acetone/workspace / edges_fwd: edge :RUNS_ON from Service[String("identity")] to Host[String("db2")] has no target node
fsck: 1 error(s), 0 advisory(ies)
error: repository has integrity errors
```

Repair means editing the graph until it is consistent again — which of the
two sides was "right" is your call, not acetone's. Here the retirement
stands, so the edge must go. Since Cypher cannot see a dangling edge, the
recipe is: resurrect the missing endpoint, delete the edge normally, then
delete the endpoint again:

```console
$ acetone query 'CREATE (:Host {name: "db2", region: "eu-central", os: "linux"})'
1 node created
$ acetone query 'MATCH (s:Service {name: "identity"})-[r:RUNS_ON]->(h:Host {name: "db2"}) DELETE r'
1 relationship deleted
$ acetone query 'MATCH (h:Host {name: "db2"}) DELETE h'
1 node deleted
```

Now the commit re-validates cleanly and writes the two-parent merge commit:

```console
$ acetone commit -m "merge bump-identity"
committed beaae79967b545ac82fc27c2f812d4702b39e958
$ acetone status
On branch main
HEAD: beaae79967b545ac82fc27c2f812d4702b39e958
workspace: clean
nodes: 12, edges: 14, schema entries: 7
$ acetone query 'MATCH (s:Service {name: "identity"}) RETURN s.version'
┌───────────┐
│ s.version │
├───────────┤
│ 2.5.0     │
└───────────┘
1 row
```

The full story — both merges, both branches — is in `acetone log --all`:

```console
$ acetone log --all
beaae79967b545ac82fc27c2f812d4702b39e958 merge bump-identity
merge: bce960e3498f2360d5e808b31288eb0ae7f245e0 c67b3adef6b307a471a52fe4d45be43e6b75f21d
c67b3adef6b307a471a52fe4d45be43e6b75f21d identity 2.5.0: scale out onto db2
bce960e3498f2360d5e808b31288eb0ae7f245e0 retire db2
f0f5b30d664e6dfabbaf61f9c8efa315e9681de6 identity patch release 2.4.2
d23309799d0583dc54b709db2a507a2736426acb merge decommission-app1
merge: 3e59a55c23a9c2d8be568974106ee1d412026ffb 19f7e936e8edb5ae247f04cdc026a3af9eefea7e
3e59a55c23a9c2d8be568974106ee1d412026ffb postgres upgraded to 16.4
19f7e936e8edb5ae247f04cdc026a3af9eefea7e decommission app1: move identity and billing to app3
4e04e05af4c92472342ade7ba874e9a2994d44cb asset registry: initial inventory
```

(Commits that landed in the same second are ordered topologically, ties
broken deterministically — so the two mid-branch commits may list in either
order on your replay.) The same story as a drawing is git's department:

```console
$ git log --oneline --graph --all
*   beaae79 merge bump-identity
|\
| * c67b3ad identity 2.5.0: scale out onto db2
* | bce960e retire db2
* | f0f5b30 identity patch release 2.4.2
|/
*   d233097 merge decommission-app1
|\
| * 19f7e93 decommission app1: move identity and billing to app3
* | 3e59a55 postgres upgraded to 16.4
|/
* 4e04e05 asset registry: initial inventory
```

## Time travel

Any query can run against any version, without touching the workspace or the
checked-out branch. The first form is `--at` on the CLI, taking a branch
name, a full ref name or a commit hash. At the seed commit, the old placement
is all still there:

```console
$ acetone query --at 4e04e05af4c92472342ade7ba874e9a2994d44cb 'MATCH (s:Service)-[:RUNS_ON]->(h:Host) RETURN s.name, h.name ORDER BY s.name, h.name'
┌────────────┬────────┐
│ s.name     │ h.name │
├────────────┼────────┤
│ billing    │ app1   │
│ billing    │ app2   │
│ identity   │ app1   │
│ postgres   │ db1    │
│ postgres   │ db2    │
│ storefront │ edge1  │
└────────────┴────────┘
6 rows
$ acetone query --at bump-identity 'MATCH (s:Service {name: "identity"}) RETURN s.version'
┌───────────┐
│ s.version │
├───────────┤
│ 2.5.0     │
└───────────┘
1 row
```

The second form is the `AT` clause inside Cypher itself, suffixing a `MATCH`
clause group. The refspec must be a **quoted string literal** (or a `$param`):

```console
$ acetone query "MATCH (s:Service {name: \"identity\"}) AT 'bump-identity' RETURN s.version"
┌───────────┐
│ s.version │
├───────────┤
│ 2.5.0     │
└───────────┘
1 row
```

Write it bare and the parser tells you exactly what it wants — the same
message appears for an unquoted commit hash:

```console
$ acetone query 'MATCH (s:Service) AT bump-identity RETURN s.name'
error: line 1, column 22: a refspec after AT must be a string literal or a parameter — try AT 'bump-identity' (or AT $ref)
```

### Tags

Tags make natural time-travel anchors — "the state we audited in July". A
tag's short name resolves bare, exactly like a branch name, and both kinds
of git tag work: a **lightweight** tag (`git tag <name> <commit>`) points
straight at the commit, and an **annotated** tag (`git tag -a`) points at a
tag *object*, which acetone peels through to the commit underneath —
nested annotated tags included:

```console
$ git tag inventory-v1 4e04e05af4c92472342ade7ba874e9a2994d44cb
$ acetone query --at inventory-v1 'MATCH (h:Host) RETURN count(h)'
┌──────────┐
│ count(h) │
├──────────┤
│ 5        │
└──────────┘
1 row
```

Refspecs resolve in git's own order (the first match wins): an exact
`refs/…` path, then `refs/tags/<name>`, then `refs/heads/<name>`, then a
commit hash. So in the unlikely event that one name is both a tag and a
branch, the **tag** wins — just as it would for `git rev-parse` — and the
full ref name (`refs/heads/<name>`) always reaches the branch
unambiguously.

(Git ancestry refspecs — `main~5`, `HEAD^` — are a planned convenience and
not yet resolved.)

### Who changed this? `acetone.blame`

For a single entity, `acetone.blame` walks history and lists every commit
that changed it — identity's trail through this chapter reads like a
changelog:

```console
$ acetone query "CALL acetone.blame('Service', 'identity')"
┌─────────┬──────────┬──────────────────────────────────────────┐
│ label   │ key      │ commit                                   │
├─────────┼──────────┼──────────────────────────────────────────┤
│ Service │ identity │ beaae79967b545ac82fc27c2f812d4702b39e958 │
│ Service │ identity │ f0f5b30d664e6dfabbaf61f9c8efa315e9681de6 │
│ Service │ identity │ 4e04e05af4c92472342ade7ba874e9a2994d44cb │
└─────────┴──────────┴──────────────────────────────────────────┘
3 rows
```

And `acetone.diff` is the in-query counterpart of the CLI's `diff`, yielding
one row per change with `_Added`/`_Removed`/`_Modified` virtual elements you
can filter like any other rows:

```console
$ acetone query "CALL acetone.diff('inventory-v1', 'main')"
┌──────────┬─────────┬─────────────────────────────────────────────────────┬───────────────────────────────────────────────────────────────────┐
│ kind     │ label   │ key                                                 │ node                                                              │
├──────────┼─────────┼─────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────────────┤
│ added    │ Host    │ "Host" ["app3"]                                     │ (:_Added:Host {name: app3, os: linux, region: eu-west})           │
│ removed  │ Host    │ "Host" ["db2"]                                      │ (:_Removed:Host {name: db2, os: linux, region: eu-central})       │
│ modified │ Service │ "Service" ["identity"]                              │ (:_Modified:Service {name: identity, tier: core, version: 2.5.0}) │
│ modified │ Service │ "Service" ["postgres"]                              │ (:_Modified:Service {name: postgres, tier: data, version: 16.4})  │
│ removed  │ RUNS_ON │ "Service" ["billing"] -"RUNS_ON"-> "Host" ["app1"]  │ NULL                                                              │
│ added    │ RUNS_ON │ "Service" ["billing"] -"RUNS_ON"-> "Host" ["app3"]  │ NULL                                                              │
│ removed  │ RUNS_ON │ "Service" ["identity"] -"RUNS_ON"-> "Host" ["app1"] │ NULL                                                              │
│ added    │ RUNS_ON │ "Service" ["identity"] -"RUNS_ON"-> "Host" ["app3"] │ NULL                                                              │
│ removed  │ RUNS_ON │ "Service" ["postgres"] -"RUNS_ON"-> "Host" ["db2"]  │ NULL                                                              │
└──────────┴─────────┴─────────────────────────────────────────────────────┴───────────────────────────────────────────────────────────────────┘
9 rows
```

## Where the registry stands

After this chapter the registry has `db2` retired, identity at 2.5.0, and two
merge commits in its history. The [schema chapter](schema-and-indexes.md)
builds on this state; if you want to reset instead, re-run
`asset-registry.sh` in a fresh directory. For merges gone *badly* wrong —
not conflicted, broken — see the [recovery runbook](../recovery/runbook.md).
