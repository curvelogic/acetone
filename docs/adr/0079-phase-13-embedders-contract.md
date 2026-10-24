# ADR-0079: Phase 13 — the embedder's contract

*Status: proposed — drafted by the define-the-next-phase grooming session
for Greg's review; the phase opens only on his explicit instruction ·
Date: 2026-10-24 · Bead: acetone-15ym (epic acetone-d5is)*

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

### Housekeeping and options

The phase also carries two housekeeping items. They are not gate-bearing,
but under ADR-0054 they still close within the phase.

- **The beads relay re-pin** to curve-rig `v0.1.1` (`83005c8`,
  `acetone-d5is.4`). It is a governing change, so it gets its own ADR and
  full review, and Greg merges it.
- **A Linux row** for the protocol document's wire-vs-process measurement
  (`acetone-d5is.5`).

`EXPLAIN` (`acetone-a9m`) is optional.

### Proposed exit criteria

Gate bead `acetone-d5is.6`, closed by Greg or by explicit delegation per
ADR-0067. The final wording is ratified at the opening.

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

- **Roadmap.** The roadmap gains a Phase 13 section marked *proposed*.
  Its stale open-questions sentence on parallel edges is corrected.
  Nothing leaves the unscheduled list: no item there passed the
  second-tenant test ahead of the contract gaps above.
- **Membership.** Three members keep their existing parents, because a
  cloud session's relay cannot remove a parent link:
  - `acetone-omk` and `acetone-a9m` stay under `acetone-2ck`;
  - `acetone-7qw.26` stays under `acetone-7qw`.

  They carry the `phase-13` label instead. `acetone-omk` and
  `acetone-7qw.26` are also dependencies of the gate. `acetone-a9m`, being
  optional, is joined by the label alone.
- **Claiming before the opening.** With Phase 12's gate closed, the
  members already show in `bd ready`. Each carries a note that it must
  not be claimed before Greg opens the phase.
- **Machine-interface changes.** There is one breaking change:
  `report_version` 2 and the typed `merge` conflict payload. The new
  fields, the procedure's columns and the `discard` verb are additive. All
  of it is CHANGELOG'd under `STABILITY.md`'s pre-1.0 posture. The
  protocol stays at version 1 unless review finds a breaking frame change.
- **Formats.** No on-disk format change is planned. If `acetone-7qw.26`
  adds a conflict kind, its implementer states the format impact. CHANGELOG
  0.5's `rel-wrong-type` was added at `format_version` 1 and is the
  precedent.
- **Open questions for the opening:**
  - whether the CLI should honour `GIT_AUTHOR_*` at all (proposed: yes,
    CLI only);
  - whether the global git config stays deferred (proposed: yes);
  - whether a daemon `tag` verb joins the phase (proposed: no);
  - whether checking out the current branch while the workspace is dirty
    stays refused once `discard` exists (proposed: yes);
  - whether `EXPLAIN` is in or out.
