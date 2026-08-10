# Phase 11 — Embedding and co-tenancy

*Boundary report. Prepared by the agent completing the phase's last working
bead (acetone-pz0k.5). The exit-criteria bead (acetone-j1hq.1) is Greg's to
close.*

## Headline

Phase 11 makes acetone **embeddable by non-Rust hosts** and **multi-tenant
within one repository**, the two capabilities ADR-0072 drew from the Phase 10
tenant's feedback. Three feature areas shipped, each through the shipped
interface:

1. **`acetone serve`** — a per-repository daemon over a local `0600` unix
   domain socket, speaking a length-prefixed JSON frame protocol (ADR-0074).
   A non-Rust client drives a full session — query, coin, streamed import,
   and merge — with no acetone library, only the documented wire protocol.
2. **Multi-graph co-tenancy** — *n* graphs in one repository, selected with
   `--graph`, each with its own ref namespace; fsck/gc/migrate namespace-
   scoped; cross-graph isolation property-tested.
3. **Relationship-type rename/merge** — schema evolution in the migrate
   family, including the autodeclare-ratchet repair (heal coined near-
   duplicate predicates into one).

`main` stayed green throughout; TCK conformance held at **2218/3897 with zero
failures** (Phase 11 added no Cypher surface, so the bar is held, not moved).

## Gate evidence against the exit criteria (acetone-j1hq.1)

### (1) Daemon — MET

`acetone serve` ships one process per repository over a kernel-ACL'd unix
domain socket (loopback+token documented as a last resort, ADR-0074 §1). The
core verb set is complete:

| Verb | PR | Notes |
|------|-----|-------|
| `query` (read + write, `autodeclare` coinage) | #261, #266 | streamed rows, advisory channel, typed terminal frame |
| `status` | #266 | read-only workspace summary |
| `schema-apply` | #267 | streamed as `chunk` frames — no path over the wire |
| `import` | #268 | streamed to a daemon-private tempfile — no path over the wire |
| `commit` / `branch` / `checkout` / `merge` / `resolve` | #270 | shared-workspace semantics |
| `log` / `conflicts` / `diff` / `blame` | (via `query` CALL) | reachable as `CALL acetone.*` procedures |

- **No paths over the wire** (ADR-0074 §4): the payload verbs receive bytes
  as `{"chunk"}` frames; `import` stages to a daemon-private `0700` tempfile
  the peer never names.
- **Budgets and bounds**: per-query governor + wall-clock budgets (the CLI's,
  unchanged), a `--max-concurrent` execution bound (default 4), a separate
  256 connection cap, and read/write idle timeouts against stalled peers.
- **Stale-writer-lock recovery designed and shipped** (ADR-0074 §8, #269):
  the daemon (only) breaks a lock left by a SIGKILLed writer when its pid
  names no live process, and retries once — so `acetone serve` no longer
  crash-loops on a dead writer's lock. The query write path and the
  ref-advancing verbs route through the one recovery helper (#270);
  `schema-apply`/`import` recovery is the deferred half (pz0k.7 — their write
  path goes through `anyhow`, not the typed lock error the helper keys on). A
  *live* lock is always refused, never wrongly broken.
- **Protocol/lifecycle ADR ratified-by-merge and flagged for boundary**:
  ADR-0074, a mid-phase decision (Gate-style), flagged here for retrospective
  review. Its security principles were discussed with Greg in-session
  2026-08-06.
- **Demonstrated by a non-Rust client**: `examples/acetone_daemon_client.py`
  (stdlib-only Python, ~220 lines) drives import → query → coin → schema-apply
  → status → commit → branch → merge against a live daemon through the shipped
  binary — all four of the criterion's demo actions (query, coin, import,
  merge). Exercised in CI by the `serve` integration suite.

### (2) Multi-graph — MET

Two co-tenant graphs in one repository, through the shipped CLI:

- **Second-graph init and `--graph` selection at open** — the keystone
  (acetone-j6ui.2), #263.
- **Per-graph workspace/merge-head refs** — the acetone-42d family resolved
  with no format change: refs fall back per-graph → shared-global →
  pre-ADR-0014, first write migrates (#262). The flagged CAS hazard was a
  false alarm (begin_write already decouples the CAS-expected ref value from
  the base content).
- **fsck namespace-scoped** — repairing the known single-graph wart (#260).
- **gc/migrate scoped, n>1 tested; cross-graph isolation property-tested** —
  `co_tenant.rs` / `co_tenant_cli.rs` suites.

### (3) Rename/merge — MET

Relationship-type rename and merge ship in the migrate family (acetone-lwv2,
#264): rename rewrites the schema entry and the edge keys; merge carries an
explicit, discriminator-aware same-pair collision policy; the autodeclare-
ratchet repair is demonstrated end-to-end (coin near-duplicate phrase
predicates, heal them into one).

### (4) Standing quality — MET

- `main` green throughout (build, test, clippy `-D warnings`, fmt, audit).
- TCK held at **2218/3897, zero failures** (verified this boundary).
- **Milestone security review**: see below.

## ADRs taken this phase

- **ADR-0072** — incorporating the Phase 10 tenant's feedback into the roadmap
  (the daemon + co-tenancy direction).
- **ADR-0073** — Phase 11 definition; parallel edges complete Phase 10.
- **ADR-0074** — the daemon's transport, protocol and lifecycle (the phase's
  load-bearing design; mid-phase decision, flagged here for boundary review).

## Review findings summary

Every code PR passed a fresh-subagent adversarial review (strongest tier,
isolated worktree) per the merge gate; the payload verbs and stale-lock
recovery additionally got dedicated security/race reviews. Notable catches
that reviews turned up and that were fixed in-phase:

- **#263 (multi-graph) blocker**: a half-initialised 2nd graph could read
  graph A's workspace via the shared-ref fallback — fixed with a sole-graph
  fallback restriction and shared-ref migration at 2nd-graph init.
- **#262 (per-graph refs)**: two real defects — the merge-in-progress backstop
  needed the legacy merge-head fallback, and fsck had to verify the per-graph
  workspace ref — both fixed with regression tests.
- **#269 (stale-lock)**: the recovery had to be bounded to the pid-liveness
  half (safe subset); a pid-cast bug (u32→i32 sign flip on a huge pid) was
  fixed proactively; the multi-daemon safety boundary was documented.
- **#270 (ref verbs) major**: the ref-advancing write verbs initially skipped
  stale-lock recovery — fixed by routing all lock-taking verbs through one
  shared helper; error kinds unified across verbs.

### Milestone security review — CLEAN

A dedicated boundary security review (fresh subagent, strongest tier) over the
whole phase diff, with the daemon's new socket attack surface as the focus,
returned **no blocker, major, or minor findings**. It did not merely read —
it built the binary, ran a live daemon, and attacked it with a hostile
raw-socket client:

- **Framing / resource exhaustion**: a ~4 GiB length prefix is refused before
  any allocation (the cap check precedes the `vec!`); garbage and 100k-deep
  nested JSON close the connection without a crash; a mid-frame stall is
  released at the IO timeout (thread + query permit freed, not parked). The
  daemon stayed at ~11 MB RSS throughout and answered a clean `status` after
  every attack.
- **Ref/path injection**: `branch`/`checkout`/`merge` with `../../../etc/...`,
  `..`, empty, embedded `\n` and `\0` are all typed errors at the store door
  (gix `FullName` validation) — no path reached `std::fs`, no namespace escape.
- **No paths over the wire** confirmed: `import`'s source is a daemon-private
  `0700` staging file the peer never names; no peer-controlled path reaches
  the filesystem.
- **Cross-tenant isolation**: `owns_ref` is an exact-match allow-list;
  `detect_namespace` re-validates the name recovered from a marker ref (a
  hand-crafted split-namespace marker can't smuggle a path); the stale
  shared-workspace fallback fires only under `is_sole_graph()`.
- **Stale-lock recovery** matches the documented one-daemon-per-repo boundary:
  process-wide mutex serialises breaks; pid parsed as positive `i32`
  (rejecting the sign-flip); `kill(pid,0)` via nix's safe wrapper erring toward
  "live"; the poisoned-lock holder string is control/bidi-escaped and length-
  capped.
- **Supply chain**: `cargo deny` clean; the one new dependency (`nix`, `signal`
  feature only) is minimal and MIT, and every crate keeps
  `#![forbid(unsafe_code)]`.

Residuals are all pre-existing, documented, and tracked — none introduced this
phase: the two-daemon double-writer window (decision pz0k.8), the SIGTERM
socket leftover reclaimed on next start (ADR-0074 §7), and a self-directed
`id`-echo amplifier bounded by the frame cap and wall clock (no action). The
gate is ready to close on security grounds.

## Open decisions and risks for the boundary

- **acetone-pz0k.8 (DECISION, Greg)**: robust stale-lock recovery regardless
  of daemon count. The shipped recovery is safe under ADR-0074's
  one-daemon-per-repository model; two daemons on one repository (different
  sockets) is unsupported and reopens the double-writer window. Making it
  robust needs an enforced daemon-exclusivity lock at `serve` startup, or a
  stale-immune `flock`/`fcntl` advisory lock — a Greg-gated call at this
  boundary.
- **acetone-pz0k.7 (deferred)**: the stale-lock pid-**reuse** refinement
  (identity + start-time, to distinguish a reused pid from the original
  holder) and extending recovery to a per-verb granularity. Deferred because
  it needs platform-specific process introspection (`/proc` on Linux,
  `sysctl`/`proc_pidinfo` on macOS) whose only tractable forms are a
  hand-rolled unsafe FFI shim (untestable off-target, in a corruption-critical
  path) or a heavy process-info crate (`sysinfo` pulls 10+ transitive crates
  including Windows/objc2 for a unix-only daemon). The shipped subset is
  strictly safe — a still-live pid is refused, exactly as today — so a reused
  pid needs manual recovery, no worse than the status quo. Its scope depends
  on the pz0k.8 decision, so it is homed under the pz0k epic pending that call.

## Follow-ups crossing the boundary

Three P3 follow-ups under the multi-graph epic (acetone-j6ui) cross the
boundary, re-homed under that owning epic. A fresh-subagent investigation
against the shipped multi-graph code confirmed **none is a live cross-graph
data-corruption or isolation bug** — the j6ui.2 fixes (the `is_sole_graph`
fallback restriction and `migrate_shared_worktree_refs` at 2nd-graph init)
hold. Their dispositions:

- **acetone-j6ui.3 — stale shared-workspace-ref cleanup (cosmetic).** The
  correctness hazard it originally named (a graph reading another's workspace
  via the shared-ref fallback) was **closed in #263**: the fallback is gated
  on `is_sole_graph()` and the shared ref is deleted at 2nd-graph init, so
  once ≥2 graphs exist no graph consults the shared ref and it is already
  gone. The residual is a dead ref lingering on a *sole* graph that migrated
  on first write — never dangerously read (the per-graph ref always wins).
  Pure litter cleanup; the correctness scope is already delivered.

- **acetone-j6ui.1 — unscoped `fsck` over-approximates (safe by design).**
  A plain `acetone fsck` in a multi-graph repo (no `--graph`) falls back to
  the standalone `refs/heads/` prefix and walks the co-tenant's own code
  branches as commit tips — false-positive findings, never a missed one: the
  scope is a strict *superset* of every graph's namespace, so no graph's
  commits go unverified and no graph's data is touched as another's. This is
  the documented, deliberate "wider scope is the safe direction for a
  diagnostic" behaviour (`fsck.rs:387-392`); the **precise** per-graph fsck
  ships via `acetone fsck --graph <g>`. Deferred because it is a diagnostic-
  precision wart, not a correctness or isolation defect, and the proper fix
  (scope the walk to the *union* of the graphs' namespaces, with report/
  verified-set merging) is a real unit, not a trivial inline nit.

- **acetone-j6ui.4 — per-graph worktree durability anchor (FLAGGED for
  Greg).** This is the one to raise explicitly. The linked-worktree durability
  anchor (`refs/acetone/worktree-anchors/<id>`) is keyed on the worktree id
  with no graph component, so two co-tenant graphs written from the **same
  linked (non-main) worktree** share one anchor and each write clobbers the
  other's. It is *reachable* with what Phase 11 shipped (linked worktrees are
  a pre-existing capability, not later-phase work), so its deferral rests on
  **severity + fix-risk, not scope** — an honest weaker-than-usual ADR-0054
  justification:
  - *Severity is contained*: only a secondary, best-effort guarantee degrades
    — foreign-`git gc` protection of *uncommitted* linked-worktree chunks.
    Every primary guarantee holds: committed data is branch-protected;
    acetone's own gc enumerates every worktree's per-graph refs and preserves
    both graphs (the anchor is not its safety floor); the **main** worktree
    writes no anchor, so the common case is untouched. The failure needs all
    of: co-tenant multi-graph + writes from a *linked* worktree + to *both*
    graphs + *uncommitted* state + a *foreign* gc.
  - *The fix is durability-critical and non-trivial*: the anchor id is parsed
    back to a worktree id at two prune-decision sites (gc staleness,
    `repo.rs:1592`; fsck coverage, `fsck.rs:534`); a naive per-(worktree,graph)
    key makes live anchors look stale and **prunes them → data loss**. The fix
    must change both parse sites in lock-step and warrants its own reviewed
    durability unit, not a boundary-eve patch. Documented as a known
    limitation in the meantime (`refns.rs:128-139`).

## What the daemon does NOT yet do (honest scope)

- **Session isolation**: a daemon connection is a **view onto the one shared
  per-worktree workspace**, exactly like concurrent CLI processes — Greg's
  session-contract decision (2026-08-10), implemented with a view to adding
  per-connection isolated sessions later. The wire protocol is model-agnostic
  (verified in review), so that mode is a workspace-ref change, not a protocol
  change.
- **`schema` show, `fsck`, `export` verbs**: ADR-0074 §5's full verb list
  includes read-side `schema` show, `fsck`, and streamed `export`, beyond the
  gate criterion's set. They are not gate-listed and did not ship this phase;
  they layer onto the protocol unchanged when a consumer needs them.
- **`SIGTERM` graceful drain**: a refinement (ADR-0074 §7); the daemon relies
  on stale-socket reclaim for restart-after-SIGKILL, not on a signal handler.

## Boundary deliverables

- This report.
- Sprint-demo deck (Artifact, archived to `docs/demos/`).
- Live demo — driven with Greg step by step at the boundary review.

Agents do not close acetone-j1hq.1; Greg closes the gate after the review.
