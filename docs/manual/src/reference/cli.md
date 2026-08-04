# CLI reference

The `acetone` command-line workbench, command by command. Every entry below is
derived from the shipped binary's `--help` output (acetone 0.3.0) and verified
against its behaviour; `acetone <command> --help` is always the most current
source.

The top-level help groups the commands, and this chapter follows the same
grouping:

| Group | Commands |
|-------|----------|
| Everyday | `init`, `status`, `commit`, `log`, `branch`, `checkout`, `diff`, `merge`, `resolve` |
| Schema | `declare-label`, `declare-rel-type`, `declare-index`, `reindex`, `schema` |
| Data & query | `import`, `export`, `query`, `shell` |
| Maintenance | `fsck`, `gc`, `migrate` |
| Plumbing | `put-node`, `get-node`, `put-edge`, `list-nodes`, `rekey` |

## Global behaviour

### `--repo`

Every command takes `--repo <REPO>` (default `.`): the path to the repository,
or any subdirectory of it — the enclosing repository is discovered by walking
up parents, like `git -C`. `init` ignores it when given its own `PATH`
argument.

### `--json`

The read commands `status`, `log`, `branch`, `diff`, `list-nodes`, `get-node`
and `schema` take `--json` and emit a single machine-readable JSON document.
Property values, labels and commit text are escaped on the way out, so hostile
graph data never reaches the terminal raw.

> **Stability:** the JSON *shape* is **unstable pre-1.0** and may change at any
> minor release, with the change noted in the CHANGELOG. The CLI is its own
> product surface, separate from the frozen `acetone-core` library API (see
> [`STABILITY.md`](https://github.com/curvelogic/acetone/blob/main/STABILITY.md)).
> Pin your acetone version if you script against exact field names or nesting.

### `--at`

`query` and `schema` take `--at <ref>` — a branch or tag short name, full ref
name or commit hash — to read a past version without checking it out:
whole-query time travel. Refspecs resolve in git's order (exact `refs/…` path,
then tags, then branches, then a commit hash — with one divergence: a ref
whose *name* is itself 40 hex characters resolves as a ref, where git would
ignore it and use the object), and annotated tags are peeled to their target
commit. With no `--at`, the current workspace state is used.

### `--format`

`query`, `export` and `import` take `-f/--format`. For `query` the output
formats are `table` (default), `json` and `csv`; for `export` and `import`
the file formats are `csv`, `json` and `ndjson`.

### Exit codes

Verified against the binary:

- **0** — success.
- **1** — an operational error; the message goes to stderr prefixed `error:`.
  Two deliberate uses: `get-node` on a miss exits 1 (with `--json` it also
  prints `null` to stdout, so a script can parse the result *and* detect
  absence from the exit code), and `fsck` exits non-zero when any
  error-severity finding exists.
- **2** — a usage error (unknown command or flag, missing argument), with the
  usage text on stderr.

### Relationship to git

An acetone version *is* a git commit, so the top-level help closes with a
rule of thumb worth restating here:

- **Use acetone, not git** for anything that writes acetone state: `commit`,
  `merge`, `resolve`, `checkout`, `declare-*`, `reindex`, `import`, `export`,
  `fsck`, `gc`, `migrate` — the git equivalents would write commits acetone
  cannot read.
- **Either works** for `log`, `status`, `diff`, `branch` — acetone's are
  graph-aware; plain git still works on the same repository.
- **Git only** for transport: `clone`, `fetch`, `push`, `pull`, `remote` —
  acetone has no transport commands of its own; any git remote is backup and
  transport.

## Everyday commands

### `acetone init [OPTIONS] [PATH]`

Create a new acetone repository in `PATH` (default: `--repo`, or `.`).

- `--object-format <sha1|sha256>` — hash function for the new repository
  (default `sha1`).
- `--co-tenant <GRAPH>` — create the graph as a **co-tenant** of an existing
  git repository, on its own ref namespace
  (`refs/heads/acetone/<GRAPH>/*`) alongside the code, instead of a
  standalone repository. `GRAPH` names the graph.

### `acetone status`

Show the current branch, head commit and workspace state (clean, dirty or
merging). Takes `--json`.

### `acetone commit -m <MESSAGE> [--trailer KEY=VALUE]...`

Turn the workspace's staged changes into a commit. Refuses when the workspace
has no changes since HEAD — including, on a brand-new repository, an empty
root commit; there is no `--allow-empty` yet. `--trailer` adds a `KEY=VALUE`
commit trailer and may be repeated. During a merge, `commit` re-validates and
completes the merge.

### `acetone log [--all]`

Show commit history, newest first. By default follows the current branch's
first-parent chain — the branch's own changelog, in which merged-in branch
commits do not appear. `--all` covers the whole commit graph instead: every
commit reachable from any branch, each exactly once, newest first in a
deterministic topological order, with a `merge: <parent> <parent>` line on
merge commits. Takes `--json` (each entry always carries its `parents`).

### `acetone branch [NAME [REFSPEC]] [--delete NAME]`

With no argument, list branches. With `NAME`, create that branch — at the
current head commit by default, or at `REFSPEC` (a branch short name, full
ref name or commit hash); the new branch is not checked out.
`--delete NAME` (`-d`) deletes a branch instead: ref removal only — no
commit or graph data is deleted, so the branch's commits stay reachable by
hash — and the checked-out branch is refused. Takes `--json`.

### `acetone tag [NAME [REFSPEC]] [-m <MESSAGE>] [--delete NAME]`

With no argument, list the graph's tags (short names). With `NAME`, create
an **annotated** tag — on the current head commit by default, or on
`REFSPEC` (a branch or tag short name, full ref name or commit hash) — with
`-m` for a message (default: the tag name). `--delete NAME` (`-d`) deletes
a tag instead: ref removal only, the tagged commit stays reachable by hash.
Takes `--json`.

Tags are written to the graph's own namespace, so their short names resolve
in `--at`/`AT` and `gc` and `migrate` manage them — in co-tenant mode a
plain `git tag` would land in the code repository's namespace instead. The
command is deliberately thin: signing, verification and the rest of git's
tag porcelain remain git's job, at the same namespaced path (ADR-0059).

### `acetone checkout <BRANCH>`

Switch the checked-out branch.

### `acetone diff <FROM> <TO>`

Show the graph-level difference between two versions (branch short names,
full ref names or commit hashes): the nodes and relationships added (`+`),
removed (`-`) or modified (`~`) from `FROM` to `TO`. Takes `--json`.

### `acetone merge [REF] [-m <MESSAGE>] [--abort]`

Merge another version into the current branch. The workspace must be clean
and a branch checked out; fast-forwards when possible, otherwise a clean
three-way merge writes a two-parent merge commit. Conflicts — cell-level (a
key edited incompatibly) or graph-level (a dangling edge or broken schema
constraint) — enter a merge-in-progress state: resolve cell conflicts with
`acetone resolve --all-ours|--all-theirs` or by writing the entities, repair
graph violations by editing the graph, then `acetone commit` to complete (it
re-validates first). `merge --abort` discards the merge and restores the
pre-merge branch tip; `REF` may be omitted only with `--abort`.

### `acetone resolve --all-ours|--all-theirs`

Resolve the cell conflicts of a merge in progress by taking every conflicted
value from one side, then `commit` to complete the merge. Cell conflicts
only — graph-level violations are repaired by editing the graph, and `commit`
re-validates before completing.

## Schema commands

### `acetone declare-label <LABEL> --key <KEY>... [--type <PROP>:<TYPE>]... [--require <PROP>]... [--unique <PROP>]...`

Declare a primary label's key, property types and constraints. `--key` names
a key property; repeat it, in order, for a composite key. Declaring the label
is required before Cypher `CREATE`/`MERGE` can persist nodes of it (node
identity is `(primary label, key tuple)` — Invariant #3).

`--type` declares a property's type as `<property>:<type>`, where the type is
one of `bool`, `int`, `float`, `string`, `bytes`, `date`, `time`, `datetime`,
`duration`, `list`. Naming the same property twice is an error rather than
last-wins. `--require` adds an existence constraint; `--unique` adds a
uniqueness constraint on a non-key property. All three may be repeated.

Declared types and constraints are enforced at write time; a property that is
absent or null satisfies any declared type (presence is `--require`'s
concern). A declared type is also what lets an index seek serve an equality
pin on a string — without it the query declines to a scan, because a `bytes`
or temporal value compares equal to its own text rendering and a raw probe
would miss it (ADR-0066).

Redeclaring a label replaces its whole constraint set, and a declaration that
existing nodes of the label would violate is refused with the violating nodes
named — backfill first, declare after.

### `acetone declare-rel-type <RTYPE> [--type PROPERTY:TYPE]...`

Declare a relationship type — required before Cypher can create relationships
of this type under a declared schema. `--type <property>:<type>` (repeatable)
declares a property's type, enforced exactly as node property types are: on
write, at declare time against existing relationships, and re-validated at
merge (acetone-7qw.12). Unlike node types, no seek relies on them — indexes
are node-only — so they guard the declaration's honesty rather than query
results, and declared types do not close the shape (ADR-0070).

Redeclaring a relationship type **replaces its whole definition**, and the
CLI cannot express a discriminator or existence constraint — so redeclaring a
type that was given a discriminator through the library would drop it, and is
therefore **refused while relationships of that type exist** (change it with
an explicit `migrate`).

### `acetone schema apply <FILE> [--dry-run]`

Apply a declarative schema document — the exact shape `acetone schema
--json` exports, closing define/import/export with acetone's own JSON as
the interchange format (no cross-vendor standard exists; GQL graph types
are the future alignment target). `-` reads stdin. The document is diffed
against the current schema and each entry reported (`add` / `change` /
unchanged); only additions and changes are staged, **in one transaction**,
so a change any declare-time check refuses — key or discriminator
stability, the type backfill check against existing data — rejects the
whole document and nothing lands. Entries in the repository but absent
from the document are left untouched and counted: **`apply` never
removes** (removal stays with `migrate`, keeping schema deliberate
history). Re-applying an applied document stages nothing and leaves the
workspace clean. Unknown fields and unknown type names are refused —
typo protection for hand-edited documents (note: a *duplicated JSON
field* within one object is last-wins, per JSON parsing convention —
only duplicate entry names are detected). A label entry with
`"surrogate": true` declares a `KEY SURROGATE` label — the first CLI
surface for surrogate declaration. Creating a node of a surrogate label
through Cypher mints its `_id` — a ULID (spec §2): time-ordered, 26
characters — visible to the creating query's own `RETURN`; an explicit
`_id` in the property map is respected. Minting is non-deterministic by
design (like a commit timestamp), so concurrent creation on two
branches merges as distinct nodes.

**Within an entry the document is desired state**: an entry named in
the document *wholly replaces* the repository's definition, so omitting
`types`, `required` or `unique` for an existing label **drops them** —
the plan names what a change drops (`change (drops unique ("serial"))`)
so a `--dry-run` shows it before anything lands. The entry-level rule
is the opposite: entries absent from the document are never touched.

`schema apply` can also express what the imperative commands cannot: a
relationship type **with a discriminator**. Redeclaring such a type
with `declare-rel-type` would drop the discriminator, and is refused
while relationships of the type exist.

`--dry-run` prints the plan and applies nothing. It diffs only — the
declare-time data checks (type backfill, require/unique violations) run
at apply, so a clean dry run is a plan preview, not a promise the apply
will be accepted.

### `acetone declare-index <NAME> --label <LABEL> --property <PROPERTY>...`

Declare a property index `idx/<name>`. The index is built from the current
nodes and maintained transactionally thereafter; it accelerates equality
lookups. Repeat `--property`, in order, for a **composite** index — its key
is the ordered tuple of those property values. Indexes are null- and
NaN-blind.

### `acetone reindex`

Rebuild every declared index from the nodes map. A no-op when the indexes are
already consistent; repairs any divergence `fsck` reports.

### `acetone schema [--at <REF>]`

Show the declared schema, grouped into labels (with their ordered key tuple
and existence/unique constraints), relationship types, and property indexes.
Read-only; `--at` inspects any version without checking it out. Takes
`--json`.

## Data & query commands

### `acetone import -f <csv|json|ndjson> <SOURCE> ...`

Import a source file, recording provenance trailers
(`Acetone-Source`/`-Extractor`/`-Source-Hash`) and detecting a no-op when the
source is unchanged. Requires a clean workspace — declare and `commit` the
target label's schema (and any relationship type) first. Declared
constraints (`--require`, `--unique`) are enforced over the imported rows
exactly as on the Cypher write path: any violation fails the whole import
atomically, with the violations listed.

- **Node mode**: `--label <LABEL>` maps each row to a node; the label's
  declared key selects which fields form the node key.
- **Edge mode**: `--edge <RTYPE>` maps each row to a relationship; requires
  `--from LABEL=field[,field...]` and `--to LABEL=field[,field...]` (the
  fields carry each endpoint's key, in key order), with `--disc <FIELD>`
  naming an optional discriminator field.
- `--branch <BRANCH>` imports onto that branch in isolation, leaving the
  current branch unchanged (created if absent, appended to if present).
- `-m/--message` overrides the synthesised commit message.

### `acetone export -f <csv|json|ndjson> [-l <LABEL> | --edge <RTYPE>] [-o <OUT>]`

Export node tables per label and edge tables per type — the inverse of
`import`: exporting then importing into a fresh repository with the same
schema reproduces identical map roots. With `--label` or `--edge`, export a
single table (to stdout, or to `-o <file>`); with neither, `-o` names the
directory to write one table per label and type into.

### `acetone query <CYPHER> [--at <REF>] [-f table|json|csv] [--param KEY=VALUE]... [--timeout <SECONDS>] [--autodeclare]`

Run an openCypher read query (alias: `acetone cypher`). `--at` reads a past
version — whole-query time travel. `--param KEY=VALUE` (repeatable) binds
the query's `$KEY`; VALUE is parsed as a Cypher literal — a number, quoted
string, `true`/`false`, `null`, or a list/map of literals — so quoting and
typing match the same text written inline (a bare unquoted word is an error,
never silently a string; see the
[cookbook](../working/query-cookbook.md#parameters)). KEY may be written
with or without the `$` sigil; a `--param` the query never uses is accepted
silently, as in Neo4j. Advisories (non-error
diagnostics, e.g. a match on an undeclared label) go to stderr and never
affect rows or exit status.

`--timeout <SECONDS>` sets the query's wall-clock budget — **60 seconds by
default**, `0` disables it. The deterministic resource caps (the resource
governor) apply regardless; the timeout bounds how long they may take to be
reached on a large stored graph, and a query it cuts off fails with a typed
error naming the flag. The library API sets no timeout by default — this is
a CLI-surface backstop (ADR-0069).

`--autodeclare` opts this invocation into **relationship-type autodeclare**
(ADR-0060, off by default): a write may use a relationship type the schema
does not declare in `CREATE`/`MERGE` position, and the type is appended to
the schema — an empty definition: no discriminator, no property types — in
the **same transaction as the data**, announced by an advisory. Reads never coin
a type: an unknown type in a match position stays an error, flag or no
flag, so a typo'd `MATCH` cannot silently mutate the schema. While a
merge is unresolved a coining write is refused outright — a conflicted
schema entry is absent from the merged schema, so coinage would silently
resolve the conflict with the empty definition; resolve the merge first.
Declaration stays deliberate by default; this is the sanctioned fast path
for experimentation and open-vocabulary ingestion.

### `acetone shell [--timeout <SECONDS>]`

Start an interactive Cypher shell (readline REPL). Enter queries — read or
write — to run them against the current workspace state; a write advances the
workspace (commit separately with `acetone commit`). Conveniences:
`:checkout <ref>`, `:log`, `:format <table|json|csv>`,
`:param <name> <literal>` (bind `$name` for later statements; bare `:param`
lists, `:param-clear` drops), `:autodeclare on|off` (let writes coin
relationship types for the rest of the session — ADR-0060, off by default;
bare `:autodeclare` shows the state; the toggle is session-scoped and
survives `:checkout`), `:quit`. Each statement runs under the same
default 60-second wall-clock budget as `acetone query`; start the shell with
`--timeout <SECONDS>` to change it (`0` disables).

## Maintenance commands

### `acetone fsck`

Verify repository integrity: manifest decode, chunk reachability and
prolly-tree structure for every version reachable from workspaces, branches
and tags; edge-map symmetry, index consistency and declared-constraint
breaches as advisories; and a history-independence spot-check (a
non-canonical map is an error). Exits non-zero when any error-severity
finding exists. See the [recovery runbook](../recovery/runbook.md).

### `acetone gc`

Consolidate the object store into a self-contained packfile: delta-rewrite
chunks against their predecessors and prune superseded loose objects and
packs. Representation-only — preserves every object exactly. Run periodically
after churn to reclaim space.

### `acetone migrate [--min-bytes N] [--mask-bits N] [--max-bytes N]`

Rewrite all history under new chunk parameters, producing new hashes
(ADR-0025). A version-preserving re-chunk — `format_version` is unchanged —
that re-encodes every version and rebuilds the commit graph, preserving each
commit's message, author and committer. Each flag defaults to the
repository's current value, so a no-flag `migrate` re-chunks under the same
parameters (a repair that leaves hashes unchanged, by history-independence).
Requires a clean, non-merging workspace, which it resets to the rewritten
head.

## Plumbing commands

Low-level single-entity tools, useful for scripting and repair. They
deliberately cover less than Cypher does: `put-node`, `get-node` and `rekey`
handle single-column keys only, and `put-edge` sets no properties and no
discriminator. Key arguments are parsed as an integer if they look like one,
else as a string.

### `acetone put-node <LABEL> <KEY> [--prop KEY=VALUE]...`

Insert or replace a node. `--prop` sets a non-key property and may be
repeated. Declared constraints (`--require`, `--unique`) are enforced as on
the Cypher write path; a write to an undeclared label stays raw.

### `acetone get-node <LABEL> <KEY>`

Look up a node by label and key. With `--json`, prints the node object — or
`null` on a miss, with a non-zero exit.

### `acetone put-edge <SRC_LABEL> <SRC_KEY> <RTYPE> <DST_LABEL> <DST_KEY>`

Insert or replace an edge between two existing nodes.

### `acetone list-nodes [-l <LABEL>]`

List nodes in key order, optionally restricted to one primary label. Takes
`--json`.

### `acetone rekey <LABEL> <OLD_KEY> <NEW_KEY> -m <MESSAGE>`

Change a node's key. A key change is modelled as delete-plus-create in one
commit (Invariant #3: `SET` can never change a key); incident edges are
rewritten onto the new key.
