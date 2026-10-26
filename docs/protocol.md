# The acetone daemon protocol, version 1

*The canonical cross-language surface (ADR-0076): a host embeds acetone by
speaking this protocol to `acetone serve`, over a unix socket or stdio.
The Python example client (`examples/acetone_daemon_client.py`) is the
reference implementation. This document describes protocol major
**1** as of the 0.8 development head (the next release); the governing design record is ADR-0074,
with ADR-0076 (protocol-first strategy) and ADR-0077 (daemon
exclusivity).*

## Compatibility

Pre-1.0, the protocol is **CHANGELOG-guarded**: every breaking change to
a frame shape is noted in the release that makes it (`STABILITY.md`'s
machine-interface promise). The protocol **major** in the hello changes
only by ADR. History of note within major 1:

- **0.7**: the `status` verb's `ok` body was reshaped to be exactly the
  `acetone status --json` document — it gained `schema_entries`,
  `workspace` (`"clean"`/`"dirty"`) and the `merge` block, reports the
  branch by short name, and dropped the `dirty` boolean. 0.6.0 clients
  reading `dirty` must read `workspace` instead.
- **0.7**: `CALL acetone.blame` yields a fourth column, `subject`.
- **0.8**: the `query` verb accepts `parameters` (additive).
- **0.8**: the `merge` verb's `conflicts` are typed items, not
  rendered strings, and the change report is `report_version` 2
  (breaking; see "The change report").

## Transports and lifecycle

**Unix socket** — `acetone serve --socket <PATH>`. The socket is created
mode `0600`; the kernel's ACL **is** the authentication: the daemon
holds no auth code and trusts every connected peer, treating only their
*data* as untrusted. On start the daemon prints exactly one readiness
line to stdout — `{"ready": true, "pid": <n>, "protocol": 1}` — then
serves. A socket path that is already served by a live daemon is
refused; a dead daemon's leftover socket is reclaimed. On SIGTERM the
daemon drains: stops accepting, unlinks the socket, completes each
connection's in-flight request, then exits 0 — waiting up to a grace
period (default 30 s, `ACETONE_SERVE_DRAIN_GRACE_SECS`) for handlers,
after which stragglers are abandoned and the exit is still 0. A second
SIGTERM forces an immediate nonzero exit. On clean exit the socket is unlinked.

**Stdio** — `acetone serve --stdio`, the LSP child-process pattern: the
host spawns the daemon and owns the pipe. Stdout carries **nothing but
frames** — there is no readiness line; the server-first hello is the
readiness signal. Logs go to stderr. Graceful shutdown is the host
closing stdin: the in-flight request completes and the process exits 0.

**One daemon per worktree** (ADR-0077): at startup the daemon takes a
kernel lock (released on process death); a second `serve` on the same
worktree — either transport — fails fast naming the running holder.

## Framing

Every frame is a 4-byte **big-endian length** followed by that many
bytes of **UTF-8 JSON** (one object). Frames above **16 MiB** are
refused at the framing layer; an oversized inbound frame closes the
connection. Requests and responses are JSON objects; responses echo the
request's `id` verbatim. Three error frames can arrive **without** an
`id`: `busy` (sent in place of the hello by a daemon at its connection
cap), `protocol-mismatch`, and a connection-setup `internal` error — a
client must tolerate an id-less error as its first frame.

## Hello

The daemon speaks first:

```json
{"acetone": {"protocol": 1, "version": "<crate version>"}}
```

The client replies `{"acetone": {"protocol": 1}}`. A protocol mismatch
is answered with a terminal `error` frame (kind `protocol-mismatch`)
and the connection closes.

## Requests and responses

A request is `{"id": <any>, "verb": "<name>", "params": {…}}`. The
response is a sequence of **streamed frames** (zero or more), then
exactly one **terminal frame**: `{"id", "ok": {…}}` or
`{"id", "error": {"kind", "message"}}`. One request at a time per
connection; a client wanting parallelism opens connections (socket
transport). Connections are views onto the **one shared per-worktree
workspace**, exactly like concurrent CLI processes.

Streamed frame vocabulary (each carries the request's `id`):

| Frame | Direction | Meaning |
|---|---|---|
| `{"row": {"columns", "values"}}` | out | one query result row |
| `{"advisory": <string>}` | out | the stderr channel (did-you-mean, plan lines) |
| `{"chunk": <string>}` | in | a piece of a streamed text/bytes payload |
| `{"chunk_end": true}` | in | ends an inbound payload stream |
| `{"chunk": <string>}` | out | a piece of a streamed result (export, report) |
| `{"table": {"name", "kind", "rows"}}` | out | announces a whole-graph export table |
| `{"finding": <string>}` | out | one fsck finding |

## Verbs

- **`query`** `{cypher, parameters?, autodeclare?, at?}` — read or write.
  Rows stream as `row` frames, advisories as `advisory` frames; the
  terminal `ok` carries `{"rows": n}` plus, for a write, a `write`
  object with the summary counts (`nodes_created`,
  `relationships_created`, `properties_set`, `labels_added`,
  `labels_removed`, `nodes_deleted`, `relationships_deleted`).
  `autodeclare: true` opts a write into relationship-type coinage
  (ADR-0060). `at: "<refspec>"` runs a **read** against that version
  (git ancestry syntax accepted, e.g. `main~1`); a write with `at` is
  refused (`write-at-version`). The `CALL acetone.log/diff/blame/
  conflicts` procedures are reachable through this verb.
  `parameters` is a JSON object `{name: value}` bound as `$name`, the
  socket's counterpart of the CLI's `query --param` — pass untrusted
  input this way rather than interpolating it into `cypher`; it binds as
  data, never as Cypher. A name may carry the `$` sigil, as with
  `--param`; binding one name both ways is refused. Values map as:
  `null` → `NULL`; a boolean → `BOOLEAN`; a string → `STRING` (never
  parsed as a temporal); an array → `LIST`; an object → `MAP`
  (recursively); a number written without a fraction or exponent and
  within the 64-bit signed range → `INTEGER`, and any other number →
  `FLOAT` (so `1.0` is a float). A number whose value is integral and
  at or beyond ±2^63 is refused however it is written: JSON parsing
  cannot tell `10000000000000000000` from `1e19`, so an out-of-range
  integer is never silently a float (the cost is that floats of that
  magnitude cannot be bound; pass such a value as a string and convert
  it in the query). For the same reason a negative zero (`-0` or
  `-0.0`) is refused: `--param` types the two differently, and once
  parsed they are the same number. A name given twice in one JSON
  object is not detected (the parser keeps the last value), whereas
  `--param` refuses a second binding. Temporal and bytes values have no parameter form on
  either channel. An ill-typed `parameters` is `bad-request`, naming
  the parameter, before the query runs; a `$name` the query uses but
  `parameters` does not bind is the CLI's `exec` error (`missing
  parameter 'name'`). Parameters compose with `at` and `autodeclare`,
  and count toward the request frame's cap.
- **`status`** `{}` — terminal `ok` body is exactly the
  `acetone status --json` document: `{branch, head, workspace, nodes,
  edges, schema_entries, merge}` (`merge` is
  `{in_progress, conflicts_remaining}` or null).
- **`schema`** `{at?}` — terminal `ok` body is exactly the
  `acetone schema --json` document (`labels`, `relationship_types`,
  `indexes`). With `schema-apply` this closes the read-modify-apply
  loop, which is the daemon's incremental schema path; there are
  deliberately no `declare-*` verbs.
- **`schema-apply`** `{dry_run?}` + inbound chunks — the schema
  document's text streams in as `chunk` frames ending with
  `chunk_end`; plan lines stream back as `advisory` frames; terminal
  `ok` is `{"applied": n}` (`{"dry_run": true, "applied": 0}` for a dry
  run). Refused while a merge is unresolved (by design). **No paths
  cross the wire** (ADR-0074 §4) — this and `import` receive bytes,
  never file names.
- **`import`** `{format, label?|edge?, from?, to?, disc?, branch?,
  message?, batch_size?}` + inbound chunks — the source streams to a
  daemon-private file the peer never names; terminal `ok` carries
  `{"imported": true, "commit", "nodes", "edges"}` or
  `{"imported": false}` for a no-change import.
- **`commit`** `{message, trailer?, allow_empty?}` → `ok {"commit":
  <hex>}`. `trailer` is an array of `"Key=value"` strings recorded as
  commit trailers.
- **`branch`** `{name?, refspec?, delete?}` — with `name` creates (→
  `ok {"created": <name>, "at": <hex>}`); with `delete` removes (→
  `ok {"deleted": <name>, "was": <hex>}`); with neither, lists (→
  `ok {"branches": [{"name", "head"}]}`).
- **`checkout`** `{branch}` → `ok {"checked_out"}`.
- **`merge`** `{refspec, message?}` or `{abort: true}` → `ok
  {"outcome": "merged"|"fast-forward"|"up-to-date"|"conflicts"}` plus,
  for conflicts, `{"count", "conflicts": [<conflict items>]}`, the
  items exactly as the `report` verb's `conflicts.items[]` (see
  "The change report" below), and `count` is their number; an
  abort answers `ok {"aborted": true}`. Conflicts are **data**, not
  errors, resolved via `resolve`/writes then `commit` (`message`
  defaults to `"merge"`).
- **`resolve`** `{all_ours|all_theirs}` → `ok {"resolved": n}`.
- **`export`** `{format, label?|edge?}` — one table's rendered text
  streams as outbound `chunk` frames (terminal `ok {"rows": n}`); with
  neither `label` nor `edge`, every table streams, each announced by a
  `table` frame (terminal `ok {"tables": n}`). The text is rendered by
  the same code as `acetone export`; the peer names its own files. In
  the whole-graph mode, a label the CLI would refuse a filename for is
  refused on the wire too (an explicitly named single-table export is
  the escape hatch, as in the CLI).
- **`fsck`** `{}` — findings stream as `finding` frames (raw strings —
  sanitising for a terminal is the displaying side's job); terminal
  `ok {"clean", "errors", "advisories"}`. Integrity errors are data.
- **`report`** `{from, to, json?}` — the change report (property-level
  before/after, endpoint metadata, merge conflicts) streamed as
  outbound `chunk` frames: the structured JSON document with
  `json: true` (byte-identical to `acetone report --json` stdout), the
  markdown artefact otherwise. Terminal `ok {"nodes", "edges",
  "conflicts"}` counts.

## The change report

The `report` verb's JSON document (and `acetone report --json`) carries
`report_version`, currently **2**. It holds `from` and `to` (each
`{refspec, commit, subject}`), `nodes[]` (`{kind, label, key,
properties, labels}`), `edges[]` (`{kind, rel_type, src: {label, key},
dst: {label, key}, disc, properties}`), `summary`, and `conflicts`:
null with no merge in progress, else `{"in_progress": true, "items":
[…]}`. Keys are typed JSON arrays, never display strings. Each conflict
item is `kind`, then the conflicted entity in the change list's own
shape, then `property`, `base`, `ours` and `theirs`:

- a node (`cell` on a node, `missing-required`, `wrong-type`,
  `unique`): `label` and `key`, matching that node's `nodes[]` entry;
- a relationship (`cell` on an edge, `dangling-edge`,
  `rel-wrong-type`, `rel-stale-discriminator`): `rel_type`, `src`, `dst` and `disc`, matching
  its `edges[]` entry;
- a schema-key conflict: `schema_key`, the encoded key in hex.

The items are derived from `CALL acetone.conflicts()`, whose `entity`
column carries the same entity as a map, so the report, the `merge`
verb and the query surface agree. Version history: **2** (0.8) replaced
each item's display-string `key` (and, for a relationship, the type in
`label`) with the typed fields above.

An unknown verb is a typed `unknown-verb` error naming the served set;
the connection survives every typed refusal except a payload-protocol
violation (see the error-kinds section).

## Error kinds

`bad-request`, `unknown-verb`, `protocol-mismatch`, `parse`, `bind`,
`exec`, `persist`, `graph`, `locked` (the single-writer conflict —
retriable), `write-at-version`, `result-too-large`, `payload-too-large`,
`schema-apply`, `import`, `export`, `report`, `busy` (the connection
cap), `internal`, and the ref-verb refusals mapped from typed graph
errors: `dirty-workspace`, `no-such-branch`, `branch-exists`,
`no-current-branch`, `nothing-to-commit`, `merge-in-progress`,
`concurrent-modification`. New kinds may be added; clients should treat
unknown kinds as terminal errors.

A **payload-protocol violation** — an over-cap stream, a non-chunk
frame inside a payload, or EOF before `chunk_end` — answers its typed
error and then **closes the connection**: the chunk stream is
desynchronised and cannot be resumed. Every other typed refusal leaves
the connection usable.

## Budgets and bounds

Per-query budgets are the CLI's (wall-clock via `--timeout`, the
governed scan budgets); the daemon additionally caps result rows at
100,000 (it materialises results before streaming, and the peak sums
across `--max-concurrent`, default 4). A separate cap bounds open
connections (256, socket transport); on the socket transport,
read/write idle timeouts (default 30 s) close stalled peers — the
stdio transport sets none (the pipe peer is the parent by
construction). Payload verbs cap inbound streams
(schema-apply 16 MiB, import 1 GiB). Writes serialise on the
single-writer lock: a concurrent write returns `locked` to retry,
exactly as two CLI processes would; a stale lock left by a killed
writer is recovered automatically (pid- and start-time-checked on
Linux).

## Concurrency and isolation

Every connection, and every CLI process on the same worktree, shares
one repository: one workspace, one current branch, one merge state.
What one changes, the others see on their next read. There is no
per-connection workspace and no isolation *between* writers'
uncommitted work: two connections staging changes interleave them in
the same workspace, and a `commit`, `checkout` or `merge --abort` on
one connection commits or resets that shared workspace for all.

**Writers serialise; readers never wait.** The verbs that change the
workspace, the current branch or the object store — a write `query`,
`commit`, `checkout`, `merge` (and `abort`), `resolve`, `import`,
`schema-apply`, and the CLI's `gc` and `reindex` — take the
single-writer lock (a second writer gets `locked`). No read takes it: a
read proceeds while a writer holds the lock, during a merge or an abort,
and during `gc`'s repack and prune.

**What a request pins.** A read sees the graph through one immutable
manifest, chosen when the request starts, and never a later write:

| Request | Pinned at the start | Read live, as the request runs |
|---|---|---|
| `query` (read) | the workspace manifest: all graph data, schema and indexes | whether a merge is in progress (one ref read; see below); each clause-group `AT <ref>`, resolved when the clause runs; the `CALL acetone.*` procedures (`log`, `diff`, `blame`, `conflicts`), which read the refs as they run |
| `query` with `at` | `at` resolved once to a commit, and that commit's manifest | as for a read `query` |
| `query` (write) | — (runs under the lock over the then-current workspace; the save is one ref update, so a reader sees all of the write or none of it) | — |
| `schema` | one manifest (the workspace's, or `at`'s) | — |
| `export` | one workspace manifest for every table | — |
| `status` | nothing: it is assembled from separate reads | branch, head, workspace-vs-head, merge state, conflicts, counts |
| `report` | nothing: `from` and `to` are resolved once for the diff and again for their `commit` fields | the refspecs; the `conflicts` section |
| `fsck` | nothing: it walks the refs as it goes | everything |

So graph data is always consistent within a read. Merge state is not
pinned with it: a query's merge-in-progress check reads the merge-head
ref once, separately from its manifest (the check is conservative — a
spurious "in progress" only turns an index seek into a scan, never
changes a result). A `status` or `report` taken while a writer changes
the repository can combine facts from before and after the change; a
`status` racing the end of a merge can fail rather than combine them.
Retry either. Pass commit hashes rather than branch names to `report`
for a stable answer.

**Two-step ref changes.** A few operations move two refs one after the
other, and a reader between the two sees an intermediate state. Each
keeps the order that is safe if the writer dies between the steps:

- **A conflicted merge** replaces the workspace with the partial merge,
  then records the merge head. Between them, reads see the partial
  merge (and treat it as a merge), but `status` shows a dirty workspace
  with no `merge` block and `CALL acetone.conflicts()` yields nothing.
- **`merge --abort`** resets the workspace to the branch tip, then
  clears the merge head. Between them, reads see the clean tip, but
  `status` still shows a `merge` block with no conflicts remaining.
- **Completing a merge** (`commit` with a merge in progress) advances
  the branch to the merge commit, then clears the merge head. Between
  them, reads and `head` show the completed merge, but `status` still
  shows a `merge` block with no conflicts remaining.

Separately, every commit — an ordinary one, a merge completion, and a
clean `merge` — saves the workspace before it advances the branch, so a
reader between the two sees the new content as uncommitted work
(`workspace: "dirty"`) on the old head. Each intermediate state lasts
only until the writer's next step, and a writer that dies between the
steps leaves a state the next operation repairs: `merge --abort`
finishes an interrupted merge or abort, and the next `commit` clears a
merge head left by an interrupted completion.

## Measured: wire vs process-per-command

ADR-0074 promised this comparison; measured on the 0.7 development head
(release binary, macOS, 20-node repo, N=50 medians, one warm
connection vs one process spawn per operation):

```
N=50 per operation, release binary, 20-node repo
status : wire    0.52 ms | process    5.00 ms | x  9.7
query  : wire    0.26 ms | process    4.92 ms | x 18.6
```

The daemon's advantage is the process spawn plus repository open
amortised across a connection — the qualitative claim ADR-0076's ruling
rests on, now with numbers.
