# Phase 4 report — diff, merge and conflicts

*Prepared at the Phase 4 boundary for the sprint-demo review. Phase 4 is the
epic `acetone-14c` plus `acetone-8c3`. `main` is green at `53b4e7a`.*

Phase 4 turns acetone from a versioned graph you can *write* into one you can
*branch, compare, and merge* — the version-control half of the "version-
controlled property graph" promise. Two branches of a graph can now be
diffed, three-way-merged, validated, and — when they conflict — inspected and
resolved in Cypher, with the whole story visible in history and attributable
per node.

## What shipped

Eleven PRs (#53–#63), each behind a fresh-subagent adversarial review:

- **Graph-level diff** (acetone-14c.1, #53/#59). `Repository::diff(from, to)`
  classifies the prolly map diff into node/edge `Added`/`Removed`/`Modified`
  records (`GraphDiff`), surfaced three ways from one source of truth:
  `acetone diff` (CLI), `CALL acetone.diff(from,to) YIELD kind,label,key`, and
  the **virtual graph** `CALL acetone.diff(...) YIELD node WHERE '_Added' IN
  labels(node)` — each changed node as a value labelled
  `_Added`/`_Removed`/`_Modified`.
- **Three-way merge** (acetone-14c.2, #54/#55). A deterministic, symmetric
  `merge_manifests` (map-wise three-way merge, `edges_rev` rebuilt from the
  merged forward map) and the commit-graph wrapper `Repository::merge`:
  up-to-date / fast-forward / clean two-parent merge commit, merge base as the
  lowest common ancestor over the commit DAG. `acetone merge <ref>`.
- **Post-merge graph validation** (acetone-14c.3, #56). A map-clean merge is
  re-checked for referential integrity (dangling edges) and schema
  constraints (existence, UNIQUE) over the changed key set; breaches surface
  as structured `GraphViolation` conflicts, not errors.
- **Property-based merge regime** (acetone-14c.5, #57). Edge-aware generator +
  properties: determinism (Invariant #4), clean-merge symmetry, and — checked
  by an independent scan — that a clean merge over valid inputs never
  introduces a dangling edge.
- **Procedure-provider seam** (acetone-8c3, #58). `CALL acetone.*` executes
  against the repository through a `ProcedureProvider` seam, mirroring how the
  executor already resolves `AT` refs without depending on `acetone-graph`.
  `acetone.diff`/`acetone.log` implemented; the seam is the shared substrate
  for blame and conflicts.
- **Node blame** (acetone-14c.6, #60). `CALL acetone.blame(label,key)` — the
  commits that changed a node, newest first, by a first-parent walk probing
  the node map (`O(log n)` per commit).
- **Conflicts as data** (acetone-14c.4, #61/#62/#63). A conflicted merge
  enters a persistent merge-in-progress state (partial-merge workspace +
  `conflicts` map + a `MERGE_HEAD` ref); it is inspected in Cypher (`CALL
  acetone.conflicts()` and the `_Conflict` virtual subgraph), resolved by
  picking a side (`acetone resolve --all-ours|--all-theirs`) or by writing a
  merged value (an ordinary write to a conflicted key clears it), and
  finalised by `acetone commit` as a two-parent merge.

Load-Bearing Invariants held throughout: merge is a pure function of
`(base, ours, theirs)` (#4, property-tested); `edges_rev` is rebuilt from the
merged/resolved forward map, never merged independently (#5); node identity
and key encodings are untouched (#2, #3); the diff and the conflicts index are
deterministic.

## Gate evidence — roadmap Phase 4 exit criteria

> *"The flagship demo works — two branches import overlapping asset data, merge
> produces both clean results and representative conflicts, conflicts are
> inspected and resolved in Cypher, history shows the whole story; blame
> implemented for nodes."*

All elements are in place and driven end-to-end (see the live demo below):

- **Two divergent branches, clean merge** — `acetone merge` fast-forwards or
  produces a two-parent merge commit; the merged graph equals a direct build
  of the union (history independence), `fsck` clean.
- **Representative conflicts** — overlapping edits to the same node/edge
  produce cell conflicts; a delete-vs-modify produces a conflict; graph-level
  breaches (dangling edge, UNIQUE collision) are detected.
- **Inspected in Cypher** — `CALL acetone.conflicts() YIELD label, key` and
  the `_Conflict` virtual subgraph (`YIELD node WHERE '_Conflict' IN
  labels(node)`).
- **Resolved in Cypher / with `resolve`** — pick a side (`resolve
  --all-ours|--all-theirs`) or write a hand-merged value; `commit` finalises.
- **History shows the whole story** — `acetone log` shows the two-parent merge
  commit; `acetone diff` between any two versions; `read_commit` confirms the
  `[ours, theirs]` parents.
- **Blame for nodes** — `CALL acetone.blame('N', k)` attributes every change.

The one exit-criterion caveat is scope: graph-level *violations* (dangling
edge / constraint) are **detected and reported** but, unlike cell conflicts,
are **not yet resolvable** — a violating merge leaves the repository unchanged
rather than entering an unexitable merge-in-progress state. Cell conflicts —
the flagship "two branches edit the same asset" case — resolve fully. The
remaining resolution paths are filed (see open risks) for Greg to weigh at the
gate.

## Decisions taken (ADRs)

Made mid-phase (decided by ADR so work proceeded), flagged here for
retrospective review:

- **ADR-0016 — post-merge graph validation.** Validation lives in the pure
  merge core (deterministic, property-covered); `MergeConflict` becomes an
  enum `Cell | Graph`; only merge-*introduced* breaches are reported.
- **ADR-0017 — procedure-provider seam and the 8c3 split.** The seam design;
  the virtual-graph surface split out because its invocation was underspecified
  and its `_Conflict` half depended on unbuilt beads.
- **ADR-0018 — diff virtual-graph query surface.** `CALL acetone.diff YIELD
  node` chosen over stateful `CALL … MATCH` and `AT`-range — most
  openCypher-standard, non-stateful, no new grammar. **Greg-chosen** from three
  options during the phase.
- **ADR-0019 — blame follows the first-parent chain** (git `--first-parent`
  semantics).
- **ADR-0020 — merge-in-progress state.** A `MERGE_HEAD` ref + a `conflicts`
  index that stores *which* keys conflict (values re-derived from ours/theirs),
  not the values; only cell conflicts are persisted.

Decision bead `acetone-jmp` tracks ADR-0016; the ADRs above are the agenda for
this gate.

## Review findings summary

Every PR was reviewed by a fresh subagent with no implementation context, at
the strongest available tier (Opus; Fable exhausted this account — ADR-0009
tiers honoured against the available set). Outcomes:

- **Ten of eleven PRs: ACCEPT or ACCEPT-WITH-NITS.** Nits were fixed inline
  (hardening tests, doc caveats, small perf) or filed as follow-ups.
- **One REQUEST-CHANGES (#61, the merge-in-progress state).** Two confirmed
  blockers — a conflicts-index entry-key collision (undercounting graph
  violations) and a graph-violation merge that could wedge the workspace with
  no exit. Both fixed (unique length-prefixed keys; graph violations no longer
  persist), re-reviewed, **ACCEPT**. The gate worked as intended.

Notable reviewer verifications: merge determinism/symmetry and
no-introduced-dangling proven by property tests; the conflict state machine's
`edges_rev` symmetry, no-false-clear, and no-wedge proven by construction and
live runs; ANSI/terminal-escape sanitisation of all new procedure and
virtual-graph output confirmed end-to-end.

## Milestone security review

A dedicated security-focused review (fresh Opus subagent) ran over the whole
Phase 4 diff (`3833de3..HEAD`): input handling, panics on untrusted data,
ref/path injection, terminal-escape injection, dependency risk. Baseline green
(build, clippy `-D warnings`, all 54 test binaries) with **no new
dependencies**.

**No blocker-class findings.** The untrusted-input surfaces are uniformly
defensive: every manifest / key / record / schema / conflicts-map decode
returns a typed error (never panics), and every commit-graph walk (`ancestors`,
`is_ancestor`, `merge_base`, `blame`, `log`) is cycle-guarded, so a hostile
clone's corrupt manifests, malformed `conflicts` map, or cyclic commit DAG
degrade to clean errors rather than panics or hangs. `delete_ref` is
name-validated and CAS-guarded, and no ref write/delete is driven by a refspec
argument (refspecs are only *read*). Merge integrity holds — an unresolved
conflict blocks commit, post-merge `validate_merged` gates clean merges against
dangling edges, and by-write resolution clears only the exact key written.

- **M1 (medium, terminal-escape injection) — FIXED.** The one unsanitised
  output path: the `UNIQUE`-violation conflict line rendered the schema
  `label`/`property` (attacker-controllable in a hostile clone) with plain
  `Display`, so crafted names could inject terminal escapes when a user ran
  `acetone merge <hostile-ref>`. Routed through `format_label` (`{:?}`-escaped),
  matching the PR #25 bar for every other Phase 4 sink. Fixed in this report's
  commit.
- **L1 (low, DoS-adjacent) — filed `acetone-vgt`.** `merge_base` is worst-case
  ~cubic over the common-ancestor set on a pathological hostile history; it
  terminates (visited-set guards), so a slow-down, not a hang. Bound it before
  large-scale untrusted use.

With M1 fixed and L1 tracked, the **Phase 4 security gate is ready to close**.

## Open risks and deferred work

Filed as beads, for prioritisation at the gate:

- **acetone-mws** (P2) — the remaining conflict-resolution surface: **merge
  `--abort`** (the escape hatch whose absence is why graph-violation merges
  are left unchanged), **graph-violation resolution** by ordinary writes, and
  mid-merge / stale-`MERGE_HEAD` recovery hardening. This is the main
  Phase-4-adjacent gap.
- **acetone-6gy** (P3) — openCypher label predicate in expression position
  (`WHERE n:Label`); today the virtual graphs use `'_Added' IN labels(node)`.
- **acetone-bvq** (P3) — `resolve_commit` git ancestry refspecs (`main~1`); the
  spec §5.2 `CALL acetone.diff('main~1', ...)` example needs it.
- **acetone-v8g** (P3) — `virtual_diff_node` per-call key-name rebuild (perf)
  and direct unit coverage (`_Removed`/schemaless/edge-null).
- **acetone-596** (P3) — `acetone.blame` multi-column key arity check (a
  composite-key blame currently returns empty rather than erroring).
- **acetone-i8z** (P3) — CALL query-shape edge cases (write-path CALL message,
  bare mid-query CALL without YIELD).

No blocker-class findings are outstanding at the time of writing (pending the
security review section above).

## The demo — branch, diff, merge, conflict, resolve, blame

The sprint demo drives the phase's actual CLI, one step per turn:

1. Seed a graph, branch `ours` and `theirs`, make **disjoint** edits, merge —
   a clean two-parent merge commit; `diff` and `log` tell the story.
2. Make **overlapping** edits (same node), merge — a conflict; `status` shows
   merge-in-progress; `CALL acetone.conflicts()` and the `_Conflict` subgraph
   inspect it in Cypher.
3. Resolve — `resolve --all-theirs`, or write a hand-merged value — then
   `commit` completes the merge.
4. `CALL acetone.blame('N', k)` attributes the node's history across the merge.
5. `fsck` clean throughout.

Greg closes the Phase 4 exit bead (`acetone-14c.7`) after the demo and review;
Phase 5 (import/export and secondary indexes) unblocks from there.
