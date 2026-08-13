# The acetone daemon protocol, version 1

*The canonical cross-language surface (ADR-0076): a host embeds acetone by
speaking this protocol to `acetone serve`, over a unix socket or stdio.
The Python example client (`examples/acetone_daemon_client.py`) is the
reference implementation. This document describes protocol major
**1** as shipped in acetone 0.7; the governing design record is ADR-0074,
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

## Transports and lifecycle

**Unix socket** — `acetone serve --socket <PATH>`. The socket is created
mode `0600`; the kernel's ACL **is** the authentication: the daemon
holds no auth code and trusts every connected peer, treating only their
*data* as untrusted. On start the daemon prints exactly one readiness
line to stdout — `{"ready": true, "pid": <n>, "protocol": 1}` — then
serves. A socket path that is already served by a live daemon is
refused; a dead daemon's leftover socket is reclaimed. On SIGTERM the
daemon drains: stops accepting, unlinks the socket, completes each
connection's in-flight request, then exits 0 (a second SIGTERM forces
an immediate nonzero exit). On clean exit the socket is unlinked.

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
request's `id` verbatim.

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

- **`query`** `{cypher, autodeclare?, at?}` — read or write.
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
- **`commit`** `{message, allow_empty?}` → `ok {"commit": <hex>}`.
- **`branch`** `{name?, refspec?}` — with `name` creates (→
  `ok {"created"}`); without, lists (→ `ok {"branches": [{name, …}]}`).
- **`checkout`** `{branch}` → `ok {"checked_out"}`.
- **`merge`** `{refspec}` or `{abort: true}` → `ok {"outcome":
  "merged"|"fast-forward"|"up-to-date"|"conflicts", …}`; conflicts are
  **data**, not errors, resolved via `resolve`/writes then `commit`.
- **`resolve`** `{all_ours|all_theirs}` → `ok {"resolved": n}`.
- **`export`** `{format, label?|edge?}` — one table's rendered text
  streams as outbound `chunk` frames (terminal `ok {"rows": n}`); with
  neither `label` nor `edge`, every table streams, each announced by a
  `table` frame (terminal `ok {"tables": n}`). The text is rendered by
  the same code as `acetone export`; the peer names its own files. A
  label the CLI would refuse a filename for is refused on the wire too.
- **`fsck`** `{}` — findings stream as `finding` frames (raw strings —
  sanitising for a terminal is the displaying side's job); terminal
  `ok {"clean", "errors", "advisories"}`. Integrity errors are data.
- **`report`** `{from, to, json?}` — the change report (property-level
  before/after, endpoint metadata, merge conflicts) streamed as
  outbound `chunk` frames: the structured JSON document with
  `json: true` (byte-identical to `acetone report --json` stdout), the
  markdown artefact otherwise. Terminal `ok {"nodes", "edges",
  "conflicts"}` counts.

An unknown verb is a typed `unknown-verb` error naming the served set;
the connection survives every typed refusal.

## Error kinds

`bad-request`, `unknown-verb`, `protocol-mismatch`, `parse`, `bind`,
`exec`, `persist`, `graph`, `locked` (the single-writer conflict —
retriable), `write-at-version`, `result-too-large`, `schema-apply`,
`import`, `export`, `report`, `busy` (the connection cap), `internal`.
New kinds may be added; clients should treat unknown kinds as terminal
errors.

## Budgets and bounds

Per-query budgets are the CLI's (wall-clock via `--timeout`, the
governed scan budgets); the daemon additionally caps result rows at
100,000 (it materialises results before streaming, and the peak sums
across `--max-concurrent`, default 4). A separate cap bounds open
connections (256, socket transport); read/write idle timeouts (default
30 s) close stalled peers. Payload verbs cap inbound streams
(schema-apply 16 MiB, import 1 GiB). Writes serialise on the
single-writer lock: a concurrent write returns `locked` to retry,
exactly as two CLI processes would; a stale lock left by a killed
writer is recovered automatically (pid- and start-time-checked on
Linux).

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
