# Glossary

The terms an acetone operator meets, in alphabetical order. Definitions
follow the
[specification](https://github.com/curvelogic/acetone/blob/main/docs/acetone-02-spec.md),
which is authoritative where they diverge.

**advisory** — a non-error diagnostic attached to a query result that never
affects its rows or exit status; for example, a `MATCH` on an undeclared
label that therefore returned no rows. The CLI prints advisories to stderr.

**anchor (chunk anchor)** — the sharded tree under `.acetone/chunks/` in
every commit and workspace tree, referencing each chunk the manifest uses, so
that git's ordinary reachability walk (and therefore `git gc`, push and
clone) keeps and transports the graph's data.

**branch** — an ordinary git branch ref pointing at an acetone commit. Listed
and created with `acetone branch`, switched with `acetone checkout`.

**canonical CBOR** — the deterministic CBOR encoding acetone uses for stored
values: definite lengths and sorted map keys, so equal values always encode
to identical bytes. Any change to it is a `format_version` bump.

**cell conflict** — a three-way merge conflict on a single cell: the same key
was edited incompatibly on both sides. Cell conflicts are data, not errors —
they are recorded in the `conflicts` map and resolved with `acetone resolve
--all-ours|--all-theirs` or by writing the entities.

**chunk** — the unit of storage: a content-addressed block (mean size about
4 KiB, configurable per repository at init) holding a piece of a prolly
tree. Chunks live in the chunk store.

**chunk store** — the content-addressed store the prolly trees are written
into. In acetone this is a git object database, which is why an acetone
repository is also a git repository.

**co-tenant** — a graph living inside an *existing* git repository, on its
own ref namespace (`refs/heads/acetone/<graph>/*`) alongside the code,
rather than in a standalone repository. Created with `acetone init
--co-tenant <graph>`.

**commit** — see *version*.

**conflicts map** — a map present only in a merge-in-progress workspace,
holding a structured record per conflict (key; base, ours and theirs values;
or the violation class for graph-level conflicts). Queryable, so conflict
triage is just querying.

**derived map** — a map exactly reproducible from its sources: `edges_rev`
(from the forward edge map) and every index (from the nodes map). `acetone
reindex` rebuilds them and must reproduce identical roots — a load-bearing
invariant.

**discriminator** — the fourth component of a relationship's identity
`(source key, type, target key, discriminator)`. It defaults to empty;
parallel relationships of the same type between the same endpoints must
declare a discriminator property in schema.

**edges_fwd / edges_rev** — the two edge maps of a version: forward
`(src, type, dst, disc) → properties`, and its derived reverse twin keyed
`(dst, type, src, disc)` for incoming-edge scans, maintained transactionally
together.

**format_version** — the on-disk format's version number, carried in every
manifest and read first on every decode. Any change to key encoding, value
encoding, chunking parameters or manifest schema increments it; it is frozen
at `1` (Gate D, ADR-0024), and old versions stay readable via
read-old-write-new dispatch (ADR-0048).

**graph violation** — a graph-*level* merge conflict, as opposed to a cell
conflict: a dangling edge (edge present, endpoint absent) or a broken schema
constraint after merging. Repaired by editing the graph; `acetone commit`
re-validates before completing the merge.

**history independence** — the load-bearing invariant that identical map
contents yield identical prolly-tree root hashes *regardless of operation
order*. It is what makes hash comparison a semantic diff: same hash, same
graph, however it was built.

**honest decline** — acetone's response to an unsupported query: a typed
"not supported" error rather than a wrong answer. The TCK conformance
classification (see the [conformance appendix](conformance.md)) never counts
a decline as a pass or a failure-by-wrong-result.

**index** — a declared property index (`idx/<name>` map) over
`(label, properties)`, built from the current nodes and maintained
transactionally thereafter; accelerates equality lookups. A composite index
keys on the ordered tuple of several property values. Indexes are null- and
NaN-blind.

**manifest** — the small canonical record that *is* a graph version: the
root hashes of its constituent maps (`schema`, `nodes`, `edges_fwd`,
`edges_rev`, indexes, and during a merge `conflicts`) plus format metadata
such as `format_version` and chunk parameters.

**memcomparable** — the order-preserving key encoding: type-tagged bytes
arranged so that byte order equals logical order, making range scans equal
label and prefix scans. Any change to it is a `format_version` bump.

**natural key** — the schema-declared key properties that give a node its
identity. Natural keys are mandatory in acetone: there are no synthetic
internal node ids, so identity survives export, import and merge.

**primary label** — the label that, together with the key tuple, forms a
node's identity: `(primary label, key tuple)` — a load-bearing invariant.
`SET` can never modify key properties; use `acetone rekey`. A node may also
carry secondary labels, which are ordinary record data.

**prolly tree** — a *probabilistic* B-tree: a search tree whose node
boundaries are chosen by content-defined chunking, so the same contents
always produce the same tree (see *history independence*) and two versions
share unchanged subtrees. The structure behind every acetone map, and what
makes diff and merge fast.

**resource governor** — the per-query budget (`QueryLimits`) the query
engine enforces — caps on result rows and other resources — so a runaway
query fails with a typed error instead of exhausting the process.

**schema** — the declared shape of the graph, stored in the `schema` map and
versioned with everything else: each label's key tuple and constraints
(existence, uniqueness), relationship types, and index declarations.

**snapshot** — an immutable read view of one version. Readers are pinned to
a root hash, so unlimited concurrent readers see stable state while the
single writer advances the workspace (snapshot isolation by construction).

**TCK** — the openCypher Technology Compatibility Kit: the executable
scenario corpus acetone's conformance is measured against on every commit.
See the [conformance appendix](conformance.md).

**three-way merge** — acetone's merge model: `merge(base, ours, theirs)` is
a pure, deterministic function over the two branches and their common
ancestor; conflicts are data in the `conflicts` map, never errors — a
load-bearing invariant.

**version** — one committed state of the graph: a git commit object whose
tree carries the manifest and chunk-anchor tree under a reserved `.acetone/`
directory. Because a version *is* a git commit, branching, tagging, transport
and signing are git-native.

**workspace** — the persistent working state of a checkout: a manifest
referenced from `refs/acetone/workspaces/<name>`, which writes advance
atomically and which survives process exit. `acetone commit` turns the
workspace's changes into a version; until then they are staged but durable.
