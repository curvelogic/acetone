# Phase 3 report — the Cypher write path and the commit discipline

*2026-07-06 · Epic acetone-mex · For Greg's review at the Phase 3 boundary.
The exit bead (acetone-mex.5) is open and is yours to close after the sprint
demo; its working deliverables are merged and the gate evidence is below.*

Phase 3 closes the workbench loop: **checkout → edit via Cypher →
status/diff → commit**. A Cypher write query now mutates the persisted
workspace atomically, under natural-key identity, with schema constraints
enforced.

## What shipped

| Bead | Deliverable | PR |
|---|---|---|
| acetone-mex.1 | **Write-execution model** (`MutableGraph` overlay + mutation log + `WriteSummary`) and the `CREATE` clause, end to end (parser, binder, executor) | #41 |
| acetone-eah | `SET` / `REMOVE` (property, replace, merge, labels) on nodes and relationships, with the bind-time key-immutability gate | #42 |
| acetone-921 | `DELETE` / `DETACH DELETE` — two-phase, clause-end connectivity check | #43 |
| acetone-k0i | `MERGE` (match-or-create) with `ON CREATE SET` / `ON MATCH SET`; in-query idempotence via the overlay | #44 |
| acetone-j5m | Overlay `EntityId`s given a reserved sentinel first byte — structurally collision-proof against storage-derived keys | #45 |
| acetone-rjf | **Per-worktree workspace ref and writer lock** (ADR-0014): git worktrees are first-class — independent writers and working state | #46 |
| acetone-huo | **Workspace chunk anchoring** (ADR-0015): the workspace ref points at a `{manifest, chunks/}` tree, so uncommitted state survives a foreign `git gc` | #47 |
| acetone-mex.2 | **Persist Cypher writes** into the workspace transaction — the Cypher→storage bridge (identity derivation, endpoint resolution, value conversion); CLI `query` runs writes; `declare-label`/`declare-rel-type` | #48 |
| acetone-mex.3 | **Constraint enforcement** (spec §2, Invariant #3): single-keyed identity, CREATE-of-existing-key, key immutability, existence, UNIQUE | #49 |
| acetone-mex.4 | **`rekey`**: a key change as an atomic delete-plus-create in one commit, rewriting incident edges | #50 |

Also delivered as part of the above: **acetone-qjq** (created-node identity
derived from the declared key at persist, closed with mex.2) and
**acetone-ady** (runtime key-immutability enforcement, closed with mex.3).

Every PR passed the mandatory adversarial review gate (fresh subagent, no
implementation context, strongest-available model tier per ADR-0009 —
`opus`, since `fable` usage is exhausted). Every review is summarised below;
every non-trivial fix commit was re-reviewed.

## Gate evidence — roadmap Phase 3 exit criteria

The roadmap sets three exit criteria. Assessed honestly:

**1. Write-feature TCK scenarios passing for the supported subset — MET**
(acetone-1h7, done at the gate on Greg's ruling that the harness verification
was required). The write subset is now verified *through the TCK harness*,
not only by unit tests:

- The harness runs each write scenario's **setup graph** (one statement at a
  time, in memory) and the query under test, then verifies both the returned
  **rows** and the openCypher **side effects** (`And the side effects should
  be:`). Side effects are read as a **graph-state delta** — distinct label
  tokens (`CREATE (:L),(:L)` is `+labels 1`), net node/relationship
  identities (`CREATE (n) DELETE n` is `+nodes 0`) and per-operation property
  counts (an overwrite is `+properties 1` *and* `-properties 1`; a null value
  is no property) — which is what openCypher actually counts.
- **Result: TCK conformance rose from 1371 to 1596 passing scenarios of
  3897** (35.2% → 41.0%), i.e. **+225 write scenarios verified**, with only
  **4 new failures** (56 total) — all genuine MERGE-relationship gaps (`SET
  r = <entity>`; a MERGE-rel `RETURN` column), filed as **acetone-q9m**.
  Write syntax beyond the v0.1 Level W subset (undirected `MERGE`, `SET
  (n).p`) is classed Unsupported, like read deferrals, not failed.
- This is *in addition to* the 134 `acetone-cypher` unit tests (full
  per-clause coverage) and the end-to-end CLI tests (create →
  read-back-in-a-fresh-process → MERGE → SET → composite keys → DELETE →
  commit → `fsck` clean, plus every constraint rejection and transactional
  atomicity).

**2. Idempotence demonstrated — MET.** Re-running a `MERGE`-based load of
identical data produces an unchanged root and `commit` reports nothing to
commit. Tested (`merge_is_idempotent_on_reexecution`) and demonstrable from
the CLI: a second `MERGE (h:Host {name:'web-01'})` prints `(no changes)`,
the node count is unchanged, and `commit` refuses ("nothing to commit").

**3. Interactive editing session captured end-to-end — MET.** The
`cypher_write_path_persists_and_stays_consistent` and
`rekey_command_changes_a_nodes_identity` CLI tests capture the full loop,
and the sprint demo drives it live. See "The demo" below.

## Decisions taken (ADRs)

Both are mid-phase decisions, made by ADR so work proceeds, and flagged
here for your retrospective review (roadmap gate discipline):

- **ADR-0014 — Per-worktree workspace ref and writer lock** (amends
  ADR-0010). The writer lock moves from the shared common git dir to the
  per-worktree git dir; the workspace becomes a single per-worktree ref
  `refs/worktree/acetone/workspace`. A throwaway probe confirmed gix 0.85
  resolves the `refs/worktree/*` namespace per-worktree exactly like git, so
  no fallback is needed. A pre-rjf repo's legacy shared ref is read as a
  fallback and migrated forward on the first write. This is what makes two
  worktrees of one clone independent — the git-native way to have two
  branches checked out at once.
- **ADR-0015 — Anchor workspace chunk sets against foreign gc** (amends
  ADR-0010/0014). The workspace ref points at a `{manifest, chunks/}` tree
  (the commit-tree shape minus the README), so git's reachability keeps the
  workspace's chunks and an uncommitted workspace survives even
  `git gc --prune=now --aggressive`. This closes ADR-0010's "commit before
  external gc" caveat. A local-only ref-plumbing change; no `format_version`
  bump. Anchoring is naive (full chunk set per save) for correctness; the
  incremental path is deferred to acetone-taf (one deviation from the huo
  bead's "build the incremental path" steer, noted).

## Review findings summary

Every PR was reviewed adversarially; blockers were found and fixed on four
of ten:

- **#41 (mex.1):** blocker — CREATE silently dropped labels/properties on an
  already-bound node; fixed (bind-time error). Two mex.2 landmines filed.
- **#42 (eah):** blocker — a later write clobbered an `AT <ref>` node
  snapshot (non-override-only refresh); fixed with an override-only lookup +
  regression test.
- **#43 (921):** blocker — a per-row connectivity check spuriously rejected
  the standard `MATCH (a)-[r]->(x) DELETE r, a` idiom; fixed by deferring the
  check to clause end.
- **#48 (mex.2):** accepted, no blockers (verified by live experiments); four
  latent findings filed (discriminator, non-round-trippable key types,
  multi-keyed-label — the last folded into mex.3).
- **#49 (mex.3):** blocker — constraints ignored same-transaction deletions,
  false-rejecting the delete-plus-create rekey path; the fix surfaced a
  deeper upsert-before-delete ordering bug; both fixed + regression test.
- **#44 / #45 / #46 / #47 / #50:** accepted, no blockers (each verified by
  targeted experiments — MERGE by independent execution, j5m by tracing the
  encoder, rjf/huo by live worktree/gc experiments, rekey by self-loop and
  parallel-edge probes).

## Milestone security review

A dedicated security review (fresh subagent, `opus`) covered the whole
Phase 3 diff — panics on untrusted data, resource exhaustion, path/ref
injection, trust posture, dependencies — verifying candidates against the
built CLI with adversarial inputs (control-character labels/data, a 20 kB
deeply-nested expression, single-statement key collisions, a 50 000-node
CREATE, a poisoned lock).

**SECURITY GATE: READY** — no blocker-class findings.

Verified safe: `NodeKey::decode` on arbitrary EntityId bytes never panics
(the `0xFF` overlay sentinel can't alias a real key); the blob-or-tree
resolver and workspace-tree parse are kind-checked, size-capped and return
typed errors; the write-clause parser/binder keeps the `MAX_AST_DEPTH`
stack-overflow guard and rejects `types[0]`-reaching malformed patterns; CLI
data flows through `sanitise_line`/`format_label`; no new crate, no process
spawn, no network, no untrusted-config read; reduced-trust posture and
local-only workspace refs preserved.

Findings, triaged:

- **LOW — `declare-label` echoed key/require/unique property names
  unescaped** (a terminal control-character injection the codebase's
  `format_label` convention exists to prevent). **Fixed** in this PR (each
  echoed name now goes through `format_label`, matching `declare-rel-type`).
- **LOW (informational) — the writer-lock holder string is unescaped on
  display.** Not reachable via the hostile-clone threat model (the lock is
  local, never transferred). Filed as **acetone-6tt**.
- **MEDIUM (known/tracked) — no query resource governor on the write path.**
  A large single-statement write materialises the workspace and stages the
  change set in memory (~353 MB for a 50 000-node CREATE), and the interim
  UNIQUE base scan is a bounded new quadratic. This is the pre-existing gap
  **acetone-iq6**; the write path reuses the read executor and does not
  meaningfully widen it, and it is self-inflicted by the operator's own
  query, not reachable from a hostile clone. Listed as an open risk.

## Open risks and deferred work

All filed as beads; none is a correctness blocker for the shipped subset,
but several are honest gaps to weigh:

- **acetone-q9m** — 4 genuine MERGE-relationship gaps the new TCK write
  verification surfaced (`SET r = <entity>`; a MERGE-rel `RETURN` column).
- **acetone-ryg** — UNIQUE is a base scan; it does not catch two *new*
  colliding nodes in one statement, so an unindexed UNIQUE can still admit a
  violating graph. Index-backed enforcement is Phase 5. *A real correctness
  gap, not just performance.*
- **acetone-cm9** — a detached-HEAD worktree is currently unusable (ADR-0014
  bootstrap only handles a branch HEAD), with a misleading error.
- **acetone-taf** — incremental workspace chunk anchoring (naive full-set
  anchoring per save is an O(total chunks) walk).
- **acetone-7vw** — persist has no guard against non-round-trippable
  (Bytes/temporal) key value types (unreachable today).
- **acetone-o8r** — `persist::edge_key` hardcodes a null discriminator
  (revisit with parallel-edge discriminators).
- **acetone-ayq** — `open()` takes the writer lock and writes a ref on the
  first open of a fresh worktree (read-path side effect).
- **acetone-in0 / acetone-2w0 / acetone-qdp / acetone-7tf** — write-path
  stat/aliasing refinements, compile-time DELETE validation, rekey polish,
  and a pre-huo-blob migration edge.
- **Pre-existing, unchanged by Phase 3:** acetone-iq6 (query resource
  governor), acetone-18z (var-length expansion bound) — the DoS surface the
  Phase 2 report already flagged; Phase 3 does not widen it materially (the
  write path reuses the same executor), but see the security review.

## The demo — the workbench loop, live

The sprint demo (deck: `docs/demos/phase-3-deck.html`, published at
<https://claude.ai/code/artifact/c2f4b218-83c4-4dd6-bb77-642166220e7b>)
drives the actual CLI step by step:

1. `acetone init` a repo; `declare-label` / `declare-rel-type` to declare
   the schema keys and types that natural-key identity requires.
2. `acetone query "CREATE (a:Host {…})-[:RUNS]->(s:Software {…})"` — the
   write persists; a fresh `acetone query "MATCH …"` reads it back (proof it
   is on disk, not in memory).
3. **Idempotent MERGE load:** run a `MERGE`-based load twice; the second run
   reports `(no changes)` and `commit` refuses.
4. **Interactive editing:** `SET` a property, `commit`, `rekey` a node
   (watch its edges follow, in one commit), and `fsck` stays clean.
5. **Constraints:** CREATE of an existing key, a UNIQUE clash, a missing
   required property, and a `SET` of a key property are each refused, and
   the workspace is left untouched.
6. **Worktrees & durability:** two `git worktree`s edit independently; a
   saved-but-uncommitted workspace survives `git gc --prune=now`.

Every stage is backed by a passing test, so the demo cannot drift from the
code.
