# ADR-0012: fsck finding taxonomy and severity model

*Status: accepted — ratified by Greg at the pre-0.1 boundary review (2026-07-11); originally an agent decision flagged for phase-boundary review · Date: 2026-07-04 · Bead: acetone-63m.7 · PR: pending*

## Context

Spec §7 lists `fsck` among the operational commands; bead acetone-63m.7
scopes a skeletal version — "chunk reachability from refs, manifest
parses, map roots resolve", with edge-symmetry and index checks
deferred to later phases. Greg's Phase 0 security review made fsck the
enforcement point for the hostile-repo checklist: it must surface
corrupted and missing chunks **distinctly**, with corrupted-input
corpora asserting error-not-panic and error-not-wrong-answer.

A verifier's output is only useful if its categories mean something
precise. Two questions had to be answered before writing it: (1) what is
the finding vocabulary — what kinds of damage exist and how are they
named — and (2) what does "healthy" mean, so that a clean repo is
unambiguous and any damage produces a signal. The load-bearing
distinction the bead calls out is MISSING (a referenced chunk is absent)
versus CORRUPT (a chunk is present but wrong); conflating them would
hide whether damage is a lost object (recoverable by refetch) or a
mutated one (a format/integrity failure).

## Decision

**A report is structured data, not a boolean.** `fsck::check(repo)`
returns an `FsckReport { findings: Vec<Finding> }`. A healthy repository
yields an **empty** findings list (`is_clean()`); `has_errors()` is true
iff any finding has `Error` severity.

**Two severities.** `Error` — the version is damaged (missing/corrupt
chunk, undecodable manifest, unreadable commit, out-of-range map root).
`Advisory` — a consistency property not yet enforced by the write path
is violated; the data is structurally intact. Edge-map symmetry (spec
§3.3) is the only advisory in this phase because the Phase 1 mutation
path maintains it by construction but nothing yet *verifies* a
hand-built or foreign repository, so a violation is a warning, not a
hard failure. Advisories still count as findings, so `is_clean()` is
false when one fires.

**Six finding kinds.**

- `MissingChunk` — a map root transitively references a chunk the store
  does not have (`get` → `Ok(None)` / `ProllyError::MissingChunk`).
- `CorruptChunk` — a chunk exists but is not a valid prolly node at its
  position in the tree (wrong level tag, keys out of order, truncated
  frames, a parent boundary the child does not honour), **or** the store
  could not return it at all (a physically damaged loose object whose
  zlib/hash check fails on read, surfacing as `ProllyError::Store`).
  Both mean "the chunk cannot be trusted as the node the tree requires",
  which is the operator-relevant fact; the underlying reason string
  preserves the distinction for diagnosis.
- `Manifest` — a manifest blob is missing, the wrong object kind, or does
  not decode under the strict decoder.
- `Commit` — a ref target or ancestor reachable from `refs/heads/*` is
  not a readable acetone commit.
- `MapRoot` — a manifest map root records a height outside `1..=MAX_HEIGHT`
  (in practice unreachable via the strict manifest decoder, which already
  validates heights; kept so the walk stays total against hand-built
  manifests).
- `EdgeAsymmetry` (advisory) — the forward and reverse edge maps disagree
  on the edge set, **or** an edge entry could not be decoded as an edge so
  symmetry could not be checked. Full semantic validation of map *contents*
  (that every key/value is a well-formed edge or index entry) is a
  later-phase concern; a decode failure encountered while computing the
  symmetry advisory is surfaced as an advisory rather than silently passed,
  so the repository does not read as clean.
- `Unverified` (advisory) — a reachable version was found but deliberately
  not verified in this phase (an annotated tag, whose tag-object peeling is
  deferred). It is named, not silently skipped: the sin a verifier must
  avoid is silence, not incompleteness.

Every chunk-level finding **names the offending chunk** (`chunk:
Option<Hash>`), including `CorruptChunk` — the walk that produces them
(`acetone_prolly::verify_reachable`) attaches the hash to every fault,
not only the missing ones.

**MISSING/CORRUPT is decided structurally, not heuristically.**
`verify_reachable` is a dedicated integrity walk: it reads each chunk and
classifies the outcome as `Missing` (the store reports absence) or
`Corrupt` (present but not a valid node at its position, or the store
could not return it). It applies the same structural checks the read
paths apply in `read_node` — level tag, parent boundary claim, inherited
lower bound — so the classification is exactly as trustworthy as the tree
reader, and a wrong-but-well-formed chunk is `Corrupt`, never a wrong
answer.

## Consequences

- **Distinctness is preserved end to end.** A lost object and a mutated
  object land in different finding kinds with the same chunk address, so
  an operator can tell "refetch this" from "this repository has been
  tampered with or has bit-rot".
- **Totality is the contract.** `verify_reachable` is a non-aborting,
  non-panicking walk: it reads every chunk (leaves included, unlike the
  anchoring walk `collect_reachable_chunks`, which only needs their
  addresses) and classifies every read outcome. It terminates and does
  bounded work on any input — a visited set plus strictly decreasing levels
  bound the descent, and each read is size-capped by the store. Hostile
  chunks, manifests and ref targets are designed to produce findings, never
  panics or a wrong "clean" — the adversarial corpus and the review of this
  change are what hold that line (the review found and fixed a real
  wrong-"clean" in an earlier revision).
- **Under-reporting beneath a corrupt parent is accepted.** A missing or
  corrupt *internal* node hides the addresses of its descendants, so
  faults strictly beneath a reported fault are not enumerated. The
  reported parent is the actionable signal; this is a bounded,
  documented limitation, not a false-clean (any damaged map still yields
  at least one finding).
- **Store read errors are treated as corruption.** A `ProllyError::Store`
  from reading a referenced chunk is classified `CorruptChunk`, not a
  propagated error. This is deliberate for a hostile-repo verifier — a
  referenced-but-unreadable object is damage — but it means a genuine
  transient I/O fault would also read as `CorruptChunk`; the reason
  string carries the underlying store error for disambiguation.
- **The advisory tier is where deferred invariants live.** As later
  phases add index maintenance and edge-symmetry enforcement to the
  write path, their fsck checks can start as advisories and be promoted
  to `Error` once the write path guarantees them, without changing the
  report shape.
- **Position checks match the read paths exactly.** The walk threads each
  node's exclusive lower bound down the tree the way `tree::get` and the
  scan cursor do — including *inheriting* the ancestor bound onto a node's
  first child — so a chunk that the read paths would reject (keys below its
  position) is a `CorruptChunk`, never a false clean. Losing that
  first-child inheritance was a real false-clean caught in review. Inner
  nodes are hash-deduplicated so a shared subtree is walked once; a chunk's
  `last_key` is content-addressed and every parent's boundary claim is
  checked, which (as the review independently verified) forces any
  misplaced shared subtree to be flagged at its shallowest reference or on
  fresh descent — so deduplication cannot hide a stricter-bound violation
  and no separate re-expansion pass is needed.
- **Deep history stays close to O(distinct chunks).** Map roots and whole
  manifests are content-addressed, so one verified clean is memoised and a
  later version reusing it is not re-walked; only genuinely new chunk sets
  cost work.
- **Scope boundaries, with follow-ups filed.** The verifier walks
  workspaces (`refs/acetone/workspaces/*`), branch history (`refs/heads/*`)
  and tags (`refs/tags/*`). A *lightweight* tag is verified like a branch; an
  *annotated* tag is reported as an `Unverified` advisory because peeling
  tag objects to their commit is deferred (`acetone-8t3`). The per-version
  map/manifest memoisation above is in place; streaming the edge-symmetry
  advisory so it need not materialise all edges per version is deferred
  (`acetone-7fe`). Neither is a correctness (false-clean) gap.
- **Revisit** when index verification and cross-map referential checks
  (every edge endpoint resolves to a node) arrive: those are new kinds,
  and some current advisories may become errors.
