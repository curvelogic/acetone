# ADR-0079: Phase 13 — the embedder's contract

*Status: accepted — drafted by the define-the-next-phase grooming session
(2026-10-24, PR #296); the phase opened by Greg first-hand in session on
2026-10-25: "Confirm. Open the phase." The open questions were ruled at
the opening (see *Rulings at the opening*) · Date: 2026-10-24, accepted
2026-10-25 · Bead: acetone-15ym (epic acetone-d5is)*

## Context

Phase 12 is ratified and its gate (`acetone-zavr.3`) is closed. The
grooming inputs were:

- the roadmap's unscheduled list;
- the three Phase 12 demo findings (`acetone-y629`, `acetone-00wh`,
  `acetone-7ldo`);
- the Phase 12 report's one next-grooming candidate: the daemon's query
  verb binds no parameters;
- the beads relay re-pin;
- five asks from the Phase 10 tenant, listed in the tenant's priority
  order in the grooming brief for this session.

Each tenant ask is in scope only if a second tenant would want it too.

**The tenant's asks, checked first against the tracker and the code.**
These are the asks ADR-0072 already answered in August. Asks 2 to 4 were
answered there as "already shipped or guaranteed".

| # | Ask | Finding |
|---|---|---|
| 1 | Daemon mode, `acetone serve --repo <path>`, owns nothing | **Shipped.** Phase 11 shipped it in 0.6 (`acetone-pz0k`, ADR-0074), and Phase 12 completed it in 0.7 (ADR-0075): socket and `--stdio` transports, verb parity, and a documented protocol. `--repo` is a global flag, so the command works exactly as the tenant wrote it. The scheduling question is therefore moot. |
| 2 | Guarantee for readers at a ref while a write or merge holds the lock | **Holds; stated only in the spec.** Reads take no lock. `WriteLock` is taken only by write, merge, merge abort, checkout, reset and `gc`. Each query reads from an immutable manifest root (spec §2: "unlimited concurrent readers, each pinned to an immutable root"). `docs/protocol.md` says only that writes serialise, and no test pins the read side. Two gaps need work. First, merge, merge abort and merge-completion commit each update the workspace or branch ref and `MERGE_HEAD` in two separate steps. Second, merge-in-progress status reads `MERGE_HEAD` live rather than from the pinned snapshot. |
| 3 | Expose parallel edges (same predicate and pair, different provenance) | **Shipped** in 0.5 (`acetone-o8r`, PRs #251/#252). A declared discriminator works through `CREATE`/`MERGE`, reads, `import --disc` and the report. The manual's CLI reference documents it (`docs/manual/src/reference/cli.md`), and the glossary defines the term. Neither `docs/user-guide.md` nor `docs/protocol.md` has a task-oriented guide to it. `acetone-7qw.26` is the one open hole: a merge can land a discriminator declaration unchecked. |
| 4 | Typed edge qualifiers | **Shipped** in 0.5: declared relationship property types are enforced (`acetone-7qw.12`, ADR-0066). |
| 5 | Rename or merge autodeclared relationship types | **Shipped** in 0.6 (`acetone-lwv2`, PR #264): `acetone rename-rel-type OLD NEW [--merge]`. |

So four asks need no work, and the fifth needs documentation and tests.
The common thread in the asks is a contract a host can rely on, and that
contract has concrete gaps:

- A host that builds queries from user input must splice strings into
  Cypher, because the socket binds no parameters.
- Every commit is authored anonymously.
- Conflict keys are display strings, in both the change report and the
  `merge` verb's reply.
- Reader isolation and parallel edges work, but nothing a host reads
  states them.
- A host that abandons a failed batch has no way to discard the
  workspace, through any interface.

Any embedder would hit each of these, so each passes the second-tenant
test.

## Decision

Phase 13 is **"The embedder's contract"** (size M, target version 0.8).
It has three strands, in rough order.

### 1. Safe, attributable writes over the socket

**Cypher parameter binding on the daemon's query verb** (`acetone-d5is.1`).
Parameters arrive as a JSON object.

- JSON values map to Cypher values under a mapping that bead specifies:
  integers within i64 become `Int`, numbers with a fraction or exponent
  become `Float`, and out-of-range or non-finite numbers are refused.
- This is a new channel, not a reuse of the CLI's. The CLI's `--param`
  takes Cypher literal text.
- Parity with the CLI is defined by a test that runs the same query
  through both interfaces, over every type both can express. Temporal and
  `Bytes` parameters are out of scope, because neither channel can
  express them today.

**A caller-supplied author** (`acetone-gid`, extended from CLI-only).

- Commits take an author through both the CLI and the socket. Annotated
  tags take one through the CLI and the library; the daemon has no tag
  verb, and adding one is not in scope.
- Identities are validated by the store's existing `validate_identity_fields`.
- When no author is given:
  - the CLI may fall back to `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`;
  - the daemon and the library never read ambient identity, and use the
    neutral signature. Otherwise the identity of whoever launched the
    daemon would be stamped on every client's commits.
- Reading the user's global git config stays deferred. It is a separate
  trust decision under ADR-0034's isolated open.

### 2. The machine-readable contract, fixed before it hardens

**Typed conflict keys** (`acetone-y629`).

- The change report's conflict items carry typed keys under
  `report_version` 2. The `merge` verb's conflict list gets the same
  treatment.
- The keys come from new, additive typed columns on
  `CALL acetone.conflicts()`. That keeps the report and the query surface
  in agreement.

**Reader isolation** (`acetone-d5is.2`).

- `docs/protocol.md` documents it, stating exactly what each request
  pins.
- Tests pin it against a held writer lock, a merge and `gc`.
- The two-step ref windows are either closed or documented.

**Parallel edges** (`acetone-d5is.3`).

- A task-oriented guide, with every example exercised by a test, that
  cross-links the manual's existing reference text, plus a note in the
  protocol document.
- The merge-time discriminator hole is closed (`acetone-7qw.26`).

### 3. Recovery and operator UX

- Workspace discard through the CLI and the socket (`acetone-omk`).
- The Phase 12 demo's two CLI findings: `acetone-00wh` (help text) and
  `acetone-7ldo` (`log --oneline`).

### Housekeeping

The phase also carries one housekeeping item. It is not gate-bearing,
but under ADR-0054 it still closes within the phase: **a Linux row** for
the protocol document's wire-vs-process measurement (`acetone-d5is.5`).

The beads relay re-pin (`acetone-d5is.4`) and `EXPLAIN` (`acetone-a9m`)
were proposed for the phase and left out of it at the opening (rulings 5
and 7 below).

### Exit criteria

Gate bead `acetone-d5is.6`, closed by Greg or by explicit delegation per
ADR-0067. Ratified as written at the opening (ruling 6).

1. Parameterised queries over the socket match the CLI's `--param` on
   every type both can express, and injection-shaped input binds
   literally. Commits carry a caller-supplied author through the CLI and
   the socket, and annotated tags through the CLI.
2. The change report's and the `merge` verb's conflict items carry typed
   keys, and the report's change ships under a bumped, CHANGELOG'd
   `report_version`. Reader isolation is documented and pinned by tests.
   Parallel edges have a tested, task-oriented guide, and `acetone-7qw.26`
   is closed.
3. Uncommitted workspace changes can be discarded through the CLI and the
   socket, and `acetone-00wh` and `acetone-7ldo` are closed.

## Consequences

- **Roadmap.** The roadmap gains a Phase 13 section, marked *proposed*
  until the opening; from 2026-10-25 it records the opening.
  Its stale open-questions sentence on parallel edges is corrected.
  Nothing leaves the unscheduled list: no item there passed the
  second-tenant test ahead of the contract gaps above.
- **Membership.** Three members keep their existing parents, because a
  cloud session's relay cannot remove a parent link:
  - `acetone-omk` and `acetone-a9m` stay under `acetone-2ck`;
  - `acetone-7qw.26` stays under `acetone-7qw`.

  They carry the `phase-13` label instead. `acetone-omk` and
  `acetone-7qw.26` are also dependencies of the gate. `acetone-a9m` was
  joined by the label alone and left the phase at the opening (ruling 5);
  a cloud session's relay cannot remove the label, so a local session
  removes it.
- **Claiming.** Before the opening, each member carried a note that it
  must not be claimed until Greg opened the phase. The members are
  claimable from the opening on.
- **Machine-interface changes.** There are two breaking changes:
  `report_version` 2, and the typed conflict payload in the `merge`
  verb's reply. The new fields, the procedure's columns and the `discard`
  verb are additive. All of it is CHANGELOG'd under `STABILITY.md`'s
  pre-1.0 posture. The protocol's major version stays at 1, per
  `docs/protocol.md`'s compatibility policy; 0.7's `status` reshape is
  the precedent.
- **Formats.** No on-disk format change is planned. If `acetone-7qw.26`
  adds a conflict kind, its implementer states the format impact. CHANGELOG
  0.5's `rel-wrong-type` was added at `format_version` 1 and is the
  precedent.
- **Re-pins.** Without `acetone-d5is.4`, the relay stays pinned to
  curve-rig `v0.1.0` (ADR-0078). A re-pin is made only when a rig change
  matters to acetone, as its own governing change.
  ADR-0078's repository-settings confirmation remains open there.

## Rulings at the opening

Greg ruled the open questions on 2026-10-25. The rulings were relayed by
the coordinating agent and recorded on PR #296
(https://github.com/curvelogic/acetone/pull/296#issuecomment-6396888196).
That comment also said that merging #296 would open the phase; the
correction after it
(https://github.com/curvelogic/acetone/pull/296#issuecomment-6397237940)
withdrew this. Greg then
confirmed first-hand in session, "Confirm. Open the phase.", the
permission to open the phase that the coordinating agent had relayed.
The full chain is quoted verbatim on the `acetone-d5is` epic.

The rulings, in the owner's terms:

1. Default the author from `GIT_AUTHOR_*` when none is given: yes, in
   the CLI only.
2. Keep reading the global git config deferred: yes.
3. Add a daemon `tag` verb: no.
4. Keep refusing a checkout over a dirty workspace now that `discard`
   exists: yes.
5. `EXPLAIN`: out of this phase.
6. Exit criteria: ratified as written.
7. Open the phase: yes, without `acetone-d5is.4` (the relay re-pin). Drop it
   from the phase. Re-pin only when a rig change matters to acetone.

Each ruling confirms or narrows *Decision* above: rulings 1–3 match
§1's author design and ruling 4 matches `acetone-omk`'s design. As
bookkeeping, `acetone-d5is.4` was closed as out of phase, and
`acetone-a9m` stays under `acetone-2ck`, outside any numbered phase.
