# Phase 1 report — Storage and model core

*2026-07-04 · Epic acetone-63m · For Greg's review at the Phase 1 boundary.
The exit bead (acetone-63m.8) is open and is yours to close; its working
deliverables are merged (PR #24) and the gate evidence is below.*

## What shipped

| Bead | Deliverable | PR |
|---|---|---|
| acetone-63m.1 | `acetone-store`: ChunkStore trait, git ODB backend (gix, reduced-trust), refs/commits | #10 |
| acetone-63m.10 | Pack-on-write validation experiment (7.1× retention win evidenced) | #13 |
| acetone-63m.2 | `acetone-prolly`: production prolly trees — build, scan, diff, three-way merge; history-independence and merge-determinism property suites | #12 |
| acetone-63m.3 | `acetone-model`: memcomparable key encoding + canonical CBOR values, golden v1 vectors | #9 |
| acetone-63m.4 | `acetone-model`: schema/node/edge/index map layouts + the manifest (`FORMAT_VERSION = 1`) | #15 |
| acetone-63m.5 | `acetone-graph`: commit plumbing, CAS-advanced workspace refs, single-writer lock, MVCC snapshots | #19 |
| acetone-63m.6 | `acetone-cli`: init/status/commit/log/branch/checkout + pre-Cypher plumbing commands | #20 |
| acetone-63m.7 | fsck (skeletal): manifest integrity, chunk reachability + structural verification, MISSING/CORRUPT distinguished, edge-symmetry advisory | #22 |
| acetone-63m.9 | `acetone-bench`: Phase 0 benchmark scenarios as regression benchmarks against the real crates | #18 |
| acetone-63m.11 | Dormant uluru MPL exception removed from deny.toml | #17 |
| acetone-63m.13 | Production pack-on-write: predecessor recording, delta/pack/idx writers, gc consolidation | #21 |
| acetone-63m.8 | Exit deliverables: `acetone fsck` CLI, `scripts/phase1-e2e.sh`, CI wiring | #24 |
| acetone-bwb | CLI terminal-injection hardening (security review HIGH-1/MEDIUM-1 + a third sink found in review) | #25 |
| — (process) | Subagent model-tier policy in CLAUDE.md (Greg-approved in-session), ADR-0009 | #16 |
| — (quality) | Record-aliasing proptest fixed (false alarm — see Review findings) | #23 |

Every PR passed the adversarial review gate with explicit reviewer
sign-off; every non-trivial fix commit was re-reviewed. Three PRs took
multiple review rounds (#21: two; #22: four; #25: two).

## Gate evidence against the roadmap's exit criteria

1. **"A scripted end-to-end — init, insert nodes/edges via plumbing,
   commit, branch, mutate, diff roots — with `git log` and `git push` to
   a real GitHub private repo working untouched"** — met.
   `scripts/phase1-e2e.sh` (PR #24) runs the full session with every step
   asserted and is executed by CI on every push. Root-level diff is
   demonstrated by manifest-blob identity: diverged branches differ at
   `<branch>:manifest`. Native interop: `git log` renders acetone commits
   (README + manifest + `chunks/` anchor tree, trailers intact) and
   `git fsck --strict` is clean. The GitHub step ran manually on
   2026-07-04 against the private repo
   `github.com/curvelogic/acetone-phase1-e2e` (created for this evidence;
   safe to delete): push of both branches, bare clone back, strict fsck
   clean, workspace ref recreated with plain `git update-ref` to
   `main:manifest`, and acetone served nodes, history and a clean fsck
   from the clone — the remote needed no knowledge of acetone (spec §3.5).
2. **"Property-test suite green (history independence, encoding order,
   merge determinism at the map level)"** — met. All 37 workspace test
   groups green in CI, including: the prolly history-independence and
   merge-determinism suites; the model encoding order/canonicity/totality
   suites plus golden format-v1 vectors; the model↔prolly acceptance test
   proving identical manifests and chunk addresses when the same content
   is built by bulk load versus reverse-order batches on separate stores;
   the graph edge-map symmetry proptest; and the bench crate's
   machine-independent invariant assertions. One flake was found and root
   caused during the phase (see Review findings, PR #23): a test bug, not
   an invariant hole — the reviewer verified Invariant 2 experimentally
   (5,000 records × every single-bit flip: zero byte-form aliasing).
3. **"fsck clean"** — met. `acetone fsck` (PRs #22 + #24) verifies
   manifest decode, chunk reachability and prolly structure for every
   version reachable from workspaces, branches and tags, distinguishes
   MISSING from CORRUPT, and exits non-zero on damage; the e2e script
   asserts a clean report before and after `git gc --prune=now`, and the
   CLI test proves the damage path (all reachable objects destroyed →
   error findings, exit 1).

## ADRs taken this phase (the agenda for boundary discussion)

- **ADR-0008** — map layouts and manifest encoding: one encoded form for
  node identity everywhere; key properties excluded from node records
  (divergence unrepresentable); version-first manifest framing.
- **ADR-0009** — subagent model tiers (Greg-approved in-session 2026-07-04;
  Gate D recorded as the revisit trigger).
- **ADR-0010** — workspace refs point at manifest blobs, CAS-advanced;
  single-writer lock with no auto-stale-break; real git HEAD as the
  checked-out ref; Phase 1 mutations are raw plumbing. Documented gap:
  uncommitted workspace chunks are not git-reachable (foreign gc hazard)
  until acetone gc protects them.
- **ADR-0011** — pack-on-write consolidation: REF_DELTA with chosen bases,
  bases-first emission, chain caps, prune gated on the written pack's own
  contents, fsync-before-prune. **Includes a deliberate spec §3.1
  amendment** (batch-pack SHOULD + gc description) folded back under this
  ADR's review — flagged here per the governing-document rule.
- **ADR-0012** — fsck finding taxonomy (Error vs Advisory; MISSING vs
  CORRUPT; scope boundaries stated honestly).

## Review findings summary

The adversarial gate (fresh reviewer, strongest model tier per ADR-0009)
caught four would-have-shipped serious defect classes this phase:

1. **PR #18** — the ported benchmark suite's amplification assert had zero
   measured headroom at 5M keys and a structurally false justification;
   the reviewer measured 20k/1M/5M itself. Replaced with an empirical
   2×height envelope (strict in smoke, warn at scale).
2. **PR #20** — the CLI silently minted no-change commits (fixed with a
   dirty-check guard); the review also exposed a real store-level defect
   from behaviour alone: `write_ref` with `expected=None` accepts a
   value-equal no-op edit (gix `MustNotExist` semantics), leaking the
   create-CAS contract → bead acetone-0ej (P2).
3. **PR #21** — three data-loss-class majors, one proven by PoC: a stack
   overflow (SIGABRT) in the delta-chain resolver on long histories; a
   prune gate that was claimed but not implemented (gated on a snapshot,
   not the written pack); and a false durability claim (no fsync before
   prune). All fixed and re-verified.
4. **PR #22** — two proven false-cleans in the verifier itself across four
   review rounds: the first-child spine didn't inherit ancestor bounds
   (fsck clean while `get`/`scan` reject), and the round-2 memoisation
   keyed on hash alone, ignoring height (a hand-built manifest reusing a
   verified hash with a wrong height read as clean). Both fixed with
   mutation-proven regression tests.

One suspected P1 invariant hole (acetone-9rw, record aliasing near ±2³²)
was investigated and **downgraded**: the flake was the test's IEEE
equality conflating `-0.0`/`+0.0`; the decoders were correct, and the
reviewer of PR #23 killed the original hypothesis by inspection and brute
force. Invariant 2 stands, now with an experimental witness.

## Open risks and boundary items

- **acetone-63m.12** (P3) — filing the gix ref-transaction TOCTOU issue
  upstream awaits your go-ahead; deliberately untouched.
- **acetone-0ej** (P2, bug) — the `write_ref` create-CAS no-op leak above.
- **acetone-zhp** (P2) — fsck cannot run when the *default* workspace
  manifest is itself the damaged object (`Repository::open` fail-fast);
  needs an open-for-fsck path.
- **acetone-8t3 / acetone-7fe** (P2) — fsck coverage/cost: annotated-tag
  peeling; cross-version chunk-set dedup (hostile-repo work
  amplification).
- **acetone-tqd** (P3, bug) — `checkout` two-ref update (workspace CAS
  then HEAD) is not atomic; wedge is recoverable; Phase 6 hardening item.
- **acetone-k78 / acetone-5a8 / acetone-5lo / acetone-627 / acetone-1qw /
  acetone-sdg** (P3–P4) — library no-change-commit guard; fsck
  anchor-completeness check; symbolic-ref visibility; consolidation
  scaling; pack benchmark; spike gix minimisation.
- **Documented format-level gap** (ADR-0010): uncommitted workspaces do
  not survive a foreign `git gc`; owned by future acetone-gc work.
- The private evidence repo `curvelogic/acetone-phase1-e2e` can be deleted
  once you've seen it.

None of these is blocker-class for closing the phase.

## Milestone security review

The dedicated phase-end security review (fresh subagent, whole phase
diff `88a03a6..main`) is recorded verbatim in
`docs/notes/phase1-security-review.md`. Verdict: **no blocker**. Every
untrusted-input decoder (CBOR, value/record/manifest/node/key, pack
index) is strict, total, allocation-bounded and panic-free, with trust
boundaries re-validated at each layer; ref/path injection is closed
through the single `validated_ref_name` door; the hostile-history DoS
paths are iterative and the 64 MiB object cap is enforced on every read.

Findings, all dispositioned before this report:

- **HIGH-1 / MEDIUM-1** (terminal injection): `acetone log` printed
  hostile-clone commit subjects/trailers raw, and `acetone fsck` printed
  findings embedding repository-controlled strings. **Fixed in PR #25**
  (bead acetone-bwb), which also closed a *third* sink the reviewer
  proved end-to-end (`get-node` secondary labels). No repository-
  controlled byte sequence now reaches the terminal raw through any CLI
  path; hostile-input tests at every sink, mutation-verified.
- **LOW-1** (CBOR array preallocation amplification) → bead acetone-8gp.
- **LOW-2** (`status` materialises all records to count) → folded into
  bead acetone-k78.
- **Residual** (bidi/zero-width visual spoofing, not control chars, so
  out of the ANSI/C1 fix's scope) → bead acetone-0ds, on record.

The load-bearing storage invariants are untouched by every finding, so
the format-determinism evidence for the gate stands.

## Process notes

- **Parallel agents require isolated worktrees.** Two early incidents of
  branches being switched underneath concurrent agents in the shared
  checkout (zero data loss); worktree isolation was made mandatory
  mid-phase and held for every subsequent unit.
- **`gh pr merge` worktree collision**: the API merge can succeed and the
  local step then abort ("'main' is already used by worktree…"); the
  reliable pattern is verify with `gh pr view`, then delete the remote
  branch via `gh api -X DELETE`.
- **ADR numbering collides under parallelism**: two in-flight PRs both
  minted ADR-0011; resolved by first-merged-keeps-it, second renumbers at
  final rebase. Worth a note in the template if it recurs.
- The subagent model-tier policy (ADR-0009) was applied throughout:
  implementation on mid tiers where scope was narrow, every review on the
  strongest available tier. The review-findings summary above is the
  evidence it pays for itself.
