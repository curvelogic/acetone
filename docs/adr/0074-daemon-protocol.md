# ADR-0074: The daemon's transport, protocol and lifecycle

*Status: accepted (agent decision per the mid-phase decision rule; the
security-model principles were discussed with Greg in-session 2026-08-06
and are recorded on `acetone-pz0k`; flagged for Phase 11 boundary review)
· Date: 2026-08-06 · Bead: acetone-pz0k*

## Context

ADR-0072/0073 committed the shape and the phase: `acetone serve`, one
process per repository, owning nothing — the host holds auth, tenancy,
repo pools, credentials and transport policy, and hands acetone a
directory. This ADR fixes the concrete decisions the first implementation
unit needs: what the daemon listens on, what travels over the wire, and
how the process behaves under supervision. The governing security
principle (Greg-discussed): *the daemon adds no authority acetone doesn't
already have as a process, and removes the temptation to add any.*

## Decisions

1. **Transport: local, kernel-ACL'd IPC only.** A unix domain socket,
   created mode `0600`, at a caller-supplied path (`--socket <path>`;
   no default inside the repository — the socket is host infrastructure,
   not repo state, and must never be committed or transferred). The
   kernel authenticates peers via filesystem permissions; acetone holds
   **no authentication code**. Ambient loopback TCP is rejected: it has
   no per-endpoint kernel ACL. A `--listen 127.0.0.1:<port> --token`
   last-resort opt-in may be added later by its own decision, and
   Windows support, when it arrives, uses named pipes (same model;
   filed separately) — the protocol below is transport-agnostic.
2. **Framing: length-prefixed JSON frames with a hard cap.** Each frame
   is a 4-byte big-endian length followed by a JSON object; frames
   above **16 MiB** are refused at the framing layer (before parsing —
   the import-bound precedent applied to the wire). JSON now, not a
   binary format: the daemon's *raison d'être* is non-Rust embedders,
   every language speaks it, and the `--json` shapes it carries already
   exist; a binary negotiation can layer behind the version handshake
   later if measurement demands it.
3. **Handshake: versioned hello.** First frame each way:
   `{"acetone": {"protocol": 1, "version": "0.6.0"}}`. The daemon
   refuses unknown protocol majors with a typed error naming what it
   speaks. The protocol version is independent of the crate version and
   changes only by ADR.
4. **Requests and streaming.** A request frame carries
   `{"id", "verb", "params"}`; the response is a stream of frames
   tagged with the request id: zero or more `{"row"}` frames, zero or
   more `{"advisory"}` frames (the CLI's stderr channel, kept separate
   from results exactly as today), then exactly one terminal
   `{"ok"| "error"}` frame — errors typed with the same identities the
   CLI renders. Payload-carrying verbs (`import`, `schema-apply`)
   receive their bytes as a stream of `{"chunk"}` frames from the
   client after the request frame: **no paths ever travel over the
   wire** — a daemon that accepted paths would let any socket peer read
   anything its uid can.
5. **Verb set.** Unit 1 ships `hello` + `query` (read). Later units add,
   against this protocol unchanged: write queries, `schema` (show +
   streamed `apply`), streamed `import`, `commit`, `branch`,
   `checkout`, `merge`, `resolve`, `status`, `log`, `conflicts`,
   `fsck`, `export` (streamed out). The CLI's one-process-per-command
   behaviours (advisories, typed errors, exit-status semantics) map
   1:1; nothing exists over the wire that the CLI cannot do.
6. **Concurrency and budgets.** Each request runs under the per-query
   budgets the CLI applies (deterministic caps + wall-clock; daemon
   wall-clock default matches the CLI's 60 s). **Two independent
   bounds**: `--max-concurrent` (default **4**) bounds simultaneous
   query *execution*, and a fixed connection cap (256) bounds *open
   connections* — an idle peer must not be able to exhaust threads by
   opening sockets that send nothing. Writes serialise additionally on
   the single-writer lock, unchanged.
   **Named memory consequence** (the honest per-query ceiling): the
   daemon *materialises* a whole result before streaming it, and a
   long-lived process does not return that peak to the OS the way
   process-per-command does — so the worst case at `--max-concurrent`
   is that peak *summed*. The daemon therefore caps result rows at
   **100,000** (well below the library default of 1,000,000); the host,
   which owns admission, sets the concurrency. A truly incremental
   (non-materialising) result path is future work if the ceiling binds.
7. **Lifecycle.** The host starts and stops the daemon. On start: bind
   the socket at a private staging path, restrict it to `0600` while it
   is unreachable, then atomically `rename` it into place (so it is
   never world-connectable); open the repository; print a single
   readiness line (`{"ready", "pid", "protocol"}`) to stdout; serve.
   **Stale-socket reclaim**: if the socket path already exists, the
   daemon connects to it — a *live* daemon there means refuse; a
   *refused* connection means a crashed daemon's leftover, which is
   unlinked and rebound. This is what makes host restart-after-SIGKILL
   work without a signal handler (the ADR-0072 crash-loop concern,
   arriving through §7). On a clean exit the `SocketGuard` unlinks;
   under `SIGKILL` the leftover is reclaimed on the next start. A
   graceful `SIGTERM` drain (stop accepting, finish in-flight, unlink)
   is a refinement for a later unit, not relied on for correctness. The
   host owns the socket path, so a *non-socket* file left there is the
   host's mistake; reclaim only unlinks an endpoint no live process is
   serving. Both read and write on a connection carry the same idle
   timeout (default 30 s, overridable by `ACETONE_SERVE_IO_TIMEOUT_SECS`)
   — the write half is what stops a slow *reader* from starving a query
   permit while its result streams out.
8. **Stale writer-lock recovery — deliberate, narrow (future unit).**
   Today a SIGKILLed *writer* leaves `acetone-writer.lock` naming a
   dead pid and recovery is a documented manual act; under supervision
   that is a crash loop. The daemon write path (only — the CLI is
   unchanged this phase) may break a stale lock. The decision, corrected
   after review:
   - **When to break** (a decision tree, not a conjunction): read the
     lock's recorded pid. If **no process with that pid exists** →
     stale, break. If a process exists but is **not an acetone process**
     (comm/argv check) → the pid was reused, stale, break. If an acetone
     process exists with that pid, compare the lock's recorded time
     against that process's *start* time: if the process started *after*
     the lock was recorded, it is a different, reused acetone pid →
     break; otherwise the lock is **live** → do not break, refuse.
   - **How to break, safely**: `O_CREAT|O_EXCL` is the primitive the
     existing lock already uses; recovery is **unlink the stale file,
     then re-create with `O_EXCL`**, and if the create loses (another
     recoverer won) re-read and re-evaluate — *not* `rename`-over,
     which replaces unconditionally and would let two recoverers both
     win (the double-writer the lock exists to prevent).
   - **Portability** (a release-target decision, not an assertion):
     pid liveness is `kill(pid, 0)` (POSIX, both targets). Process
     identity/start-time needs `/proc` on Linux and `sysctl
     KERN_PROC_PID` on macOS; the unit picks between a small `libc`
     dependency (justified in its PR) behind a thin `cfg`-gated shim, or
     a vetted cross-platform process-info crate — decided when it is
     built, with the unsafe/dependency justification the repo requires.
   - **What is compared**: the lock records its *acquisition* time
     (`lock.rs`), not the holder's process start time; the comparison
     above is acquisition-time vs process-start-time across two
     one-second-resolution clocks, and errs on the side of *not*
     breaking a live lock. The unit's tests include the pid-reuse race
     and the double-recoverer race.

## Consequences

- Unit 1 is buildable against fixed decisions: socket + framing +
  hello + read-query, integration-tested by a raw-socket client — which
  doubles as the seed of the criterion's non-Rust-client demonstration.
- The tenant's latency question (process-per-command vs daemon) becomes
  measurable as soon as unit 1 lands; the measurement is part of the
  phase's evidence.
- Choosing JSON framing defers serialisation performance; the
  versioned hello is the escape hatch, and changing it is an ADR.
- The stale-lock decision (§8) touches `acetone-graph`'s lock module and
  is the riskiest item here; it is deferred to the write-verbs unit, and
  its review should attack the pid-reuse guard and the double-recoverer
  race hardest.
- **No paths over the wire** (§4) is the load-bearing rule for the
  payload verbs not yet written; it is restated in the code at the
  verb-dispatch site so the `import`/`schema-apply` implementer meets it
  where they work, not only here.
