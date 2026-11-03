# Phase 13 — The embedder's contract

*Boundary report. Prepared by the agent completing the phase's last
working bead (acetone-d5is.5). The exit-criteria bead (acetone-d5is.6)
is the owner's to close, directly or by explicit delegation
(ADR-0067). The owner opened the phase first-hand on 2026-10-25
("Confirm. Open the phase."), with seven opening rulings recorded on
PR #296 and in ADR-0079.*

## Headline

Phase 13 fixes the contract a host embeds against before it hardens.
Writes over the socket are now safe and attributable: queries take bound
parameters, and commits carry a caller-supplied author. The
machine-readable contract is settled: the change report and the merge
verb carry typed conflict keys under `report_version` 2, reader
isolation is documented and pinned by tests, and parallel edges have a
guide, with their discriminator rule enforced at declare time, merge
time and import time. Operators can recover a workspace: they can
discard uncommitted work, and an interrupted merge abort no longer
leaves a trap. The phase landed in 17 PRs (#299–#315), each through
the adversarial review gate.

## Shipped

**Strand 1: safe, attributable writes over the socket**

| Item | Bead | PR |
|---|---|---|
| Cypher parameter binding on the daemon's `query` verb (`params.parameters`) | d5is.1 | #300 |
| Caller-supplied author: `--author` on CLI commit/import/merge/tag with a `GIT_AUTHOR_*` fallback (CLI only); `params.author` on the daemon's commit/import/merge | gid | #305 |

**Strand 2: the machine-readable contract**

| Item | Bead | PR |
|---|---|---|
| Typed conflict keys: `report_version` 2, the merge verb's items, and the `entity` column of `CALL acetone.conflicts()` | y629 | #301 |
| Report v2 review follow-ups: value forms, key order, the merge verb's error case, manual, tests | d5is.7 | #313 |
| Reader isolation documented (`protocol.md`, user guide) and pinned by tests; a cross-handle gc read race fixed along the way | d5is.2 | #304 |
| Task-oriented guide to parallel relationships | d5is.3 | #306 |
| A merged discriminator change is a conflict, judged by provenance (ADR-0082) | 7qw.26 | #303 |
| Declare-time discriminator gaps closed; renames judged the same way | d5is.9 | #311 |
| Edge import enforces the declared discriminator | d5is.13 | #307 |

**Strand 3: recovery and operator UX**

| Item | Bead | PR |
|---|---|---|
| `acetone discard` and the daemon's `discard` verb | omk | #310 |
| Crash-safe `merge --abort` ordering; writes refused while a merge is half-aborted | d5is.11 | #312 |
| Help and manual mention ancestry refspecs | 00wh | #308 |
| `acetone log --oneline` | 7ldo | #309 |

**Housekeeping**

| Item | Bead | PR |
|---|---|---|
| The owner referred to by role; a phase opens on the owner's instruction (ADR-0080) | — | #299 |
| The owner referred to by role in the remaining live material (ADR-0081) | d5is.8 / d5is.10 | #302 |
| A Linux row for the wire-vs-process measurement, with the harness checked in | d5is.5 | #314 |
| Security review fixes: author identity whitespace and size limit, `serve --help` verbs | d5is.12 | #315 |

The yanked `clipboard-win` bump (acetone-00hz, #297) landed between
the plan and the opening and is recorded on the tracker.

## Gate evidence (acetone-d5is.6's criteria)

1. **Safe, attributable writes over the socket.**
   - Parameterised daemon queries match the CLI's `--param` on every
     type both can express. Differential tests compare the two, and
     injection-shaped input binds literally (#300).
   - Commits through the CLI and the socket, and annotated tags through
     the CLI, carry a caller-supplied author (#305). The daemon never
     reads ambient identity, as the opening ruled. ✅
2. **The machine-readable contract fixed.**
   - The change report's and the merge verb's conflict items carry
     typed keys under a bumped `report_version` 2, recorded in the
     CHANGELOG as a breaking change (#301, #313).
   - Reader isolation is documented in `protocol.md` and pinned by
     tests (#304, extended in #312).
   - A tested guide to parallel edges exists (#306).
   - The merge-time discriminator-declaration hole is closed (#303).
     The phase also closed the declare-time and import-time holes it
     found along the way (#311, #307). ✅
3. **Recovery and operator UX.**
   - Uncommitted workspace changes can be discarded through the CLI and
     the socket (#310).
   - The Phase 12 demo findings are closed: 00wh (#308) and 7ldo
     (#309). ✅

Every user-facing item is reachable through the shipped interface,
following the ADR-0054 corollary. The public-API freeze job is green at
head. TCK conformance is unchanged, because the phase added no Cypher
surface.

## ADRs taken

- **ADR-0079**: the phase definition, accepted at the opening with the
  owner's seven rulings.
- **ADR-0080 / ADR-0081**: refer to the owner by role, and a phase
  opens on the owner's first-hand instruction. ADR-0080 is
  **governing** (it changes CLAUDE.md's phase-start rule) and is
  listed here for the owner's retrospective review.
- **ADR-0082**: a merged discriminator change is a conflict, judged by
  which side wrote the edge. It adds one sentence to **spec §2**,
  which is a **governing-document** change listed for review. #311
  extended its "Declare time" section.

Recorded deviations from the beads' designs, each endorsed by its
reviewer:

- **#300:** two numbers serde_json cannot type are refused rather than
  guessed: integers at or beyond ±2^63, and negative zero.
- **#301:** one `entity` map column instead of flat typed columns.
- **#305:** `rekey` and `rename-rel-type` commits keep the neutral
  author.
- **#311:** a rename now also refuses a one-sided discriminator
  mismatch.
- **#312:** writes are refused while a merge is half-aborted.

## What review caught

Every PR went to a fresh adversarial reviewer. These are the notable
catches, all fixed in-phase:

- **#311.** A rename could still store an undeclared type's edges under
  a declared discriminator, which falsified the provenance premise of
  ADR-0082. The PR's own doc comment claimed it could not. The reviewer
  reproduced it, and the fix was red-checked.
- **#312.** In the new half-aborted state, an ordinary write cleared
  the empty-conflicts mark, and the next commit landed the merge's
  content as single-parent work. That was the hazard the PR set out to
  close, coming back in mirrored form. Writes are now refused in that
  state. The reviewer also caught a `protocol.md` claim about `status`
  that the code contradicted.
- **#304.** Pinning reader isolation surfaced a real cross-handle gc
  read race, fixed in the same PR.
- **#314.** The doc's explanation of the Linux ratio was contradicted
  by its own table, and it called the measurement script "the same
  harness" when the original was never checked in.

## Milestone security review

A fresh subagent reviewed the whole phase diff (`ae918a6..c81715f`,
16 commits, 64 files) on the strongest tier. It read the code, ran the
phase's graph suites and drove `serve --stdio` with a frame probe. Its
full verdict is recorded on the phase epic (`acetone-d5is`,
2026-11-03) as the primary record. **No blocker-class findings; the
gate can close on security grounds.**

Three SHOULD-FIX findings, all fixed in-phase under acetone-d5is.12
(#315):

- An author name or email with surrounding whitespace was accepted, but
  git reads it back trimmed, so what was stored differed from what was
  read.
- An author field had no length cap: one 16 MiB frame committed a
  15 MiB name into history.
- `serve --help` omitted the `discard`, `report` and `schema` verbs.

Six notes, accepted with their rationale on the epic:

- `json_params` refuses integral floats at or beyond 2^63. This is
  documented, though the message says "integer".
- Parameter nesting is bounded by serde_json's default depth limit of
  128.
- A non-UTF-8 `GIT_AUTHOR_*` variable reads as unset.
- `report::conflict_items` indexes keys that its only builder always
  sets, so no panic is reachable.
- Socket `discard` drops every connection's staged writes. This is
  documented, gated like the other write verbs, and refused mid-merge.
- A missed object lookup retries twice at 1 ms. Only genuine misses
  pay this.

Found sound:

- identity header injection on every path;
- the neutral committer;
- frame sync on refused imports;
- parameter naming;
- the crash-safe abort;
- discriminator stability at declare, merge and import time;
- merge determinism (`format_version` 1 holds);
- derived maps under discard;
- pack retirement order.

No new crates.

## Process notes

- **Lost tracker writes, repaired.** The earlier sessions' closes for
  d5is.3, 00wh, 7ldo and omk never reached the tracker: their PRs
  merged, but the beads stayed open. They were closed retrospectively
  this session, with their PRs cited, and relayed. The bead for #307
  (`acetone-d5is.13`) was never relayed at all and does not exist on
  the tracker. The PR and its squash commit are its record.
- **A command-chaining slip, caught.** A rebase of #311 stopped on a
  CHANGELOG conflict, and a `;` in the chain let the following
  `git push` run. It pushed nothing: the local branch ref had not moved
  mid-rebase, and the remote was confirmed unchanged before the
  corrected push. This repeats the Phase 12 lesson that merge and push
  chains must use `&&` only.
- **Branches.** A cloud session cannot delete branches. The merged
  phase branches and the `beads-inbox/*` relay branches are left for
  the owner or a local session to delete.

## Queued for the boundary

- Retrospective review of the governing changes: ADR-0080's
  phase-start rule and ADR-0082's spec §2 sentence.
- Ratify the recorded deviations above.
- Tracker housekeeping that needs a local session: remove the
  `phase-13` label from acetone-a9m (EXPLAIN, out of the phase by
  ruling 5).

## Follow-ups crossing the boundary

**None.** Every bead in the phase is closed except the gate.
acetone-a9m (EXPLAIN) carries the phase label but was ruled out of the
phase at the opening, so it is not a crossing follow-up. One
non-blocking review note is recorded here rather than filed, because
it is too small to be a unit of effort: in the half-aborted merge
state, a commit that carries a write fails with the daemon's generic
error code, while a bare commit gets `merge-in-progress`.

## Open risks

The six accepted notes from the security review, above. None is
blocker-class.
