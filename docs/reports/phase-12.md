# Phase 12 — The daemon in service

*Boundary report. Prepared by the agent completing the phase's last
working bead (acetone-zavr.8). The exit-criteria bead (acetone-zavr.3)
is Greg's to close. Phase opened by Greg 2026-08-11 ("open the next
phase and get started with implementation"), with the two queued
decisions ruled by him in the same session (ADR-0076, ADR-0077).*

## Headline

Phase 12 turns the daemon from a shipped capability into **the surface a
host can live on**: full verb parity with the CLI, a second transport,
an enforced process model, a documented versioned protocol with measured
performance, the tenant's history-facing features buildable entirely
over the socket, and the multi-graph residuals closed. Seventeen PRs
(#274–#290), every one through the full adversarial review gate;
`main`'s toolchain gates are green (with one disclosed breach along the
way — see Process, below).

## Shipped

**Daemon completion (gate criterion 1)** — the 0.6.0 boundary
assessment's gap list, closed through the shipped interface:

| Item | PR |
|---|---|
| `status` frame = the `status --json` document (breaking, CHANGELOG'd) | #275 |
| Graceful SIGTERM drain; second-SIGTERM escalation | #276 |
| `export` and `fsck` verbs (chunk/finding streams, no paths on the wire) | #277 |
| `serve --stdio` — the LSP child-process transport (ADR-0076's companion) | #278 |
| `params.at` — whole-query time travel over the socket | #282 |
| `schema` verb + the documented read-modify-apply incremental path | #283 |
| Stale `serve --help` text (rider, closed acetone-6532) | #277 |

**History surfaced (criterion 2)** — the tenant's reading-diffs feature
is buildable entirely over the socket by a non-Rust client:

| Item | PR |
|---|---|
| `acetone blame` + `CALL acetone.blame` subject column | #279 |
| `acetone report` + the `report` verb (property-level deltas, conflicts, markdown/JSON) | #280 |
| Git ancestry refspecs (`main~1`, `HEAD^`, `^N`) everywhere refspecs resolve | #284 |

**Writer-lock robustness (criterion 3)** — ADR-0077 delivered in full
(PR #281): the daemon-exclusivity flock (per-worktree — a recorded,
reviewer-endorsed refinement of the ADR's wording), taken by both
transports; pid-reuse detection (Linux-complete, macOS-conservative);
`schema-apply`/`import` routed through stale-lock recovery.

**Multi-graph residuals (criterion 4)** — all three closed: per-graph
worktree durability anchors (#286, with a recorded design deviation —
git's ref D/F rule forced a separate namespace), pre-split shared-ref
cleanup (#287), multi-graph fsck as the union of graph namespaces
(#288).

**Beyond the plan** — Greg-directed mid-phase scope (recorded on the
bead): **`acetone attach`** (#285), the tenant's clone-dance killer —
`clone` + one idempotent command replaces the three-command git
plumbing every consumer README embedded. Plus one in-phase bug from a
review finding: the standalone-workspace guard now scans every worktree
(#289). And the protocol document itself (#290): `docs/protocol.md`,
protocol 1 — transports, framing, the full per-verb frame vocabulary,
the complete error-kind registry, budgets, compatibility policy — with
the **measured** wire-vs-process comparison ADR-0074 promised: on a
warm connection the daemon is ~10× (status) to ~19× (small query)
faster than process-per-command (N=50 medians, release binary;
independently re-run by the reviewer within noise).

## Gate evidence (acetone-zavr.3's four criteria)

1. **Assessment gaps closed through the shipped interface** — every
   item in the table above, each with tests driving the live socket
   (and the stdio transport, ADR-0076's companion). ✅
2. **A history-facing feature buildable entirely over the socket** —
   blame with subjects, the change report (byte-identical JSON stream),
   diffs at ancestry refspecs: all reachable by the Python reference
   client with no acetone library; test-pinned per PR. ✅
3. **Writer-lock robustness per ADR-0077** — two daemons on one
   worktree refused at startup (either transport, kernel-released on
   death); stale-lock recovery covers every daemon write path;
   pid-reuse positively detected on Linux. ✅
4. **Multi-graph residuals resolved** — j6ui.1/.3/.4 all closed with
   their reviews' demanded regression tests. ✅

TCK conformance is unchanged (the phase added no Cypher surface); the
public-API freeze gate is green at head with three deliberate,
re-blessed additions (attach, DaemonLock family, the worktree scan).

## ADRs taken

- **ADR-0075** — the phase definition (Greg-ruled at opening).
- **ADR-0076** — cross-language strategy: protocol-first; C ABI
  declined; bindings demand-driven (Greg-ruled; the protocol document
  and measurement in #290 are its deliverable).
- **ADR-0077** — daemon exclusivity via flock (Greg-ruled option a).
  **Recorded deviations**, both reviewer-endorsed: the lock is
  per-worktree (not per-repository — the race it prevents is on the
  per-worktree writer lock), and conflicts surface as a new
  `DaemonExclusive` error rather than `Locked` (whose delete-the-file
  advice would reopen the double-daemon window for a kernel lock).

## What review caught (the gate earning its keep)

Every PR passed through a fresh adversarial reviewer; the notable
catches, all fixed in-phase: a **panic on untrusted daemon input**
(multibyte ancestry suffix — found in my own review prep, then the
reviewer proved my regression test vacuous and the red-check was redone
honestly); **two blocker-class co-tenancy hazards in attach**
(layering onto a standalone repo cross-wired workspaces; a second-graph
attach orphaned a sole graph's shared-ref workspace — both empirically
demonstrated, both now guarded exactly as init guards); the union fsck
**aborting on one invalid marker** (the "diagnostic that refuses to
run" sin, reviewer-demonstrated); the durability-anchor design's
**git D/F impossibility** (found by the unit's own red test); and eight
missing kinds in the protocol document's error registry. The milestone
security review (below) found the whole surface sound.

## Milestone security review

Fresh subagent over the entire `v0.6.0..main` diff; **no blocker-class
or high-severity findings; GATE-READY.** Six low/note findings, all
triaged accepted-risk with recorded rationale (drain-expiry is
SIGKILL-equivalent by design; attach's lock-free ref surgery degrades
to typed CAS failures in every traced interleaving; the pid-namespace
assumption predates the phase; export/report memory is disk-bounded by
the CLI-equivalence model; fsck's dedup is O(n²) on damaged stores
only; a mid-import stale-lock retry is convergent under natural-key
upserts). `cargo audit` clean; the phase's one new crate (signal-hook)
justified and vetted.

## Process — a breach, disclosed

**"Main is always green" was breached this phase**: I merged PR #286
while its public-API freeze job was red (a misread check-count), and my
next two merge chains joined CI-watch and merge with `;` instead of
`&&`, so their failed watches did not gate. Main's freeze job stayed
red from #286 until #289's bless repaired it. No wrong code: the
symbols were deliberate, reviewed API — only the snapshot ratification
was missing. Disclosed in PR #289's body and on the epic at the time.
Process fixes applied: merge chains are `&&`-only.
**Boundary proposal for Greg**: make the freeze job (and
build-and-test) *required checks* on main, so a red gate blocks merges
mechanically rather than by agent discipline.

## Queued for the boundary

- Ratify ADR-0075/0076/0077's recorded deviations (per-worktree
  exclusivity; `DaemonExclusive`; the anchors' separate namespace).
- The required-checks proposal above.
- A governing-doc edit: spec §5.2's "(not yet resolved by v0.1)"
  parenthetical is stale now ancestry refspecs shipped (queued from
  PR #284's review — a spec edit needs its own reviewed change).
- The `acetone attach` mid-phase scope addition (Greg-directed;
  ADR-0073-style ratification).

## Follow-ups crossing the boundary

**None.** Every bead the phase opened or was seeded with is closed;
the grooming observation recorded on the epic (the query verb binds no
parameters — an injection-shaped parity gap for an embedder composing
queries from user input) is explicitly a *next-grooming* candidate, not
a floating follow-up, and is called out here per ADR-0054.

## Open risks

The six accepted-risk notes from the security review (above), and the
protocol document's pre-1.0 compatibility posture (CHANGELOG-guarded;
majors by ADR) — deliberate, documented, and now measured.
