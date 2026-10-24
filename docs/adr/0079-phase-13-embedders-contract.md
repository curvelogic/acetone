# ADR-0079: Phase 13 — the embedder's contract

*Status: proposed — drafted by the define-the-next-phase grooming session
for Greg's review; the phase opens only on his explicit instruction ·
Date: 2026-10-24 · Bead: acetone-15ym (epic acetone-d5is)*

## Context

Phase 12 is ratified and its gate (`acetone-zavr.3`) is closed. The
grooming inputs were the roadmap's unscheduled list, the three Phase 12
demo findings (`acetone-y629`, `acetone-00wh`, `acetone-7ldo`), the Phase
12 report's one next-grooming candidate (the daemon's query verb binds no
parameters), a relay re-pin, and five asks from the Phase 10 tenant, in
the tenant's priority order. Each ask is in scope only if a second tenant
would want it too.

**The tenant's asks, checked against the tracker and the code first:**

| # | Ask | Finding |
|---|---|---|
| 1 | Daemon mode, `acetone serve --repo <path>`, owns nothing | **Shipped.** Phase 11 (0.6, `acetone-pz0k`, ADR-0074) and completed in Phase 12 (0.7, ADR-0075): socket and `--stdio` transports, verb parity, the documented protocol. `--repo` is the global flag, so the asked-for spelling works as written. The scheduling signal is that it has already shipped. |
| 2 | Guarantee for readers at a ref while a write or merge holds the lock | **Holds, but documented only in the spec.** The read path takes no lock: `WriteLock` is taken only by the write, merge, checkout and reset paths. Every read is pinned to an immutable manifest root (spec §2: "unlimited concurrent readers, each pinned to an immutable root"). `docs/protocol.md` says only that writes serialise, and no test pins the read side. One window needs examining: a conflicted merge updates the workspace ref and `MERGE_HEAD` in two separate steps. |
| 3 | Expose parallel edges (same predicate and pair, different provenance) | **Shipped** in 0.5 (`acetone-o8r`, PRs #251/#252). A declared discriminator works through `CREATE`/`MERGE`, reads, `import --disc` and the report. Neither the user guide nor the protocol document mentions it, so it is undiscoverable. `acetone-7qw.26` (a merge can land a discriminator declaration unchecked) is the one open hole. |
| 4 | Typed edge qualifiers | **Shipped** in 0.5: declared relationship property types are enforced (`acetone-7qw.12`, ADR-0066). ADR-0072 recorded this as already shipped. |
| 5 | Rename or merge autodeclared relationship types | **Shipped** in 0.6 (`acetone-lwv2`, PR #264): `acetone rename-rel-type OLD NEW [--merge]`. |

So four of the five asks need no work, and the fifth is a documentation
and test gap. What the asks do share is the theme behind them: someone
building on acetone needs a contract they can rely on. The open gaps in
that contract are concrete. A host composing queries from user input has
to splice strings into Cypher, because the socket binds no parameters.
Every commit is authored anonymously. The change report's conflict keys
are display strings. Reader isolation and parallel edges work but are not
stated anywhere a host would look. And a host that abandons a failed
batch cannot discard the workspace through any interface. Each of these
passes the second-tenant test: any embedder would hit it.

## Decision

Phase 13 is **"The embedder's contract"** (size M, target version 0.8).
It has three strands, in rough order:

1. **Safe, attributable writes over the socket.** Cypher parameter
   binding on the daemon's query verb, through the same typed value
   channel as the CLI's `--param` (`acetone-d5is.1`). A caller-supplied
   author identity for commits and tags through the CLI and the socket,
   with commit-header injection refused (`acetone-gid`, extended from
   CLI-only). Reading the user's global git config stays deferred: it is
   a separate trust decision under ADR-0034's isolated open, and a hosted
   embedder supplies identity explicitly anyway.
2. **The machine-readable contract, fixed before it hardens.** Typed
   conflict keys in the change report, under `report_version` 2
   (`acetone-y629`). Reader isolation during a write or merge, documented
   in `docs/protocol.md` and pinned by tests, with the merge window either
   closed or documented (`acetone-d5is.2`). Parallel edges documented end
   to end with test-exercised examples (`acetone-d5is.3`), and the
   merge-time discriminator hole closed (`acetone-7qw.26`).
3. **Recovery and operator UX.** Workspace discard through the CLI and
   the socket (`acetone-omk`), and the Phase 12 demo's two CLI findings
   (`acetone-00wh` help text, `acetone-7ldo` `log --oneline`).

The phase also carries two housekeeping items that are not gate-bearing.
The first is the beads relay re-pin to curve-rig `v0.1.1` (`83005c8`,
`acetone-d5is.4`). It is a governing-document change, so it gets its own
ADR and full review, and Greg merges it. The grooming survey found the
pinned bd unchanged at 1.3.1 and rig's reusable workflow unchanged; the
relay script itself gains support for server-mode stores. The second
item is a Linux row for the protocol document's wire-vs-process
measurement (`acetone-d5is.5`). `EXPLAIN` (`acetone-a9m`) is optional
extra work if there is room.

**Proposed exit criteria** (gate bead `acetone-d5is.6`, to be closed by
Greg or by explicit delegation per ADR-0067; final wording ratified at
the opening):

1. Parameterised queries over the socket match the CLI's `--param`, and
   injection-shaped input binds literally. Commits and tags carry a
   caller-supplied author through the CLI and the socket.
2. The change report's conflict items carry typed keys under a bumped,
   CHANGELOG'd `report_version`. Reader isolation is documented and
   test-pinned. Parallel edges are documented with tested examples, and
   `acetone-7qw.26` is closed.
3. Uncommitted workspace changes can be discarded through the CLI and
   the socket, and `acetone-00wh` and `acetone-7ldo` are closed.

## Consequences

- The roadmap gains a Phase 13 section marked *proposed*. Nothing leaves
  the unscheduled list: none of its items passed the second-tenant test
  ahead of the contract gaps above.
- Three members keep their existing parent epics because a cloud
  session's relay cannot remove a parent link: `acetone-omk` and
  `acetone-a9m` (under `acetone-2ck`) and `acetone-7qw.26` (under
  `acetone-7qw`). They join the phase through the gate's dependencies and
  the `phase-13` label instead.
- The phase makes one breaking machine-interface change,
  `report_version` 2. The new author and parameter fields and the
  `discard` verb are additive. All are CHANGELOG'd under `STABILITY.md`'s
  pre-1.0 posture, and the protocol stays at version 1 unless review
  finds a breaking frame change.
- Open questions for the opening: whether to read the author from the
  user's global git config now or keep it deferred; whether checking out
  the current branch while the workspace is dirty stays refused once
  `discard` exists (proposed: yes); and whether `EXPLAIN` is in or out.
