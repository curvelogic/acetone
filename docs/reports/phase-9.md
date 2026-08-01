# Phase 9 report — at scale and in conformance

*Epic `acetone-2ck` · base `main @ 848d0e9` (post-0.3.1) · this report covers PRs #199–#230*

**This is the amended report.** The phase was first reported done at
PRs #220/#222; Greg reopened it because criterion 3's evidence had been
measured against an in-memory source rather than the shipped read path, and
wiring the seeks into the product exposed that an unselective seek *loses* to
the scan it replaces. His ruling: the real problems were that we select an
index seek when we shouldn't — the cost model needed refining — and that
`WHERE` didn't use indexes; selective lab wins were secondary to indexing
being *beneficial*. PRs #224–#230 are that remediation, and this report's
criterion-3 section now carries the shipped-path evidence. An earlier docs
amendment (PR #223) recording the interim not-satisfied state was closed as
superseded; its measurements survive below as the before-half of the story.

Phase 9 is the phase where acetone stops being demonstrably correct on small
inputs and starts being **measurably correct at size**. Two things had to
happen together: the query engine had to stop declining large parts of
openCypher (the conformance half), and the storage and verification paths had
to stop assuming everything fits in memory or in one pass (the scale half).

The headline numbers: **TCK conformance moved from 1602/3897 (41.1%) with 50
failures to 2185/3897 (56.07%) with zero failures**; import of a source larger
than memory completes in bounded resident memory; on the **shipped read path**
a selective index seek beats the scan 17× (0.27% selectivity), an absence
proof by two orders of magnitude, and a primary-key point lookup 1104× —
while unselective seeks now *decline* to within 1% of the scan instead of
losing to it by up to 37× (ADR-0065); and `fsck` and `merge_base` are
verified sub-quadratic on multi-version repositories.

Everything landed autonomously under the usual gate: per bead a design recorded
in the bead, TDD, a fresh strongest-tier adversarial review with no
implementation context, fix/re-review, squash-merge on green CI, close. That
gate did substantial work this phase — see *What review caught* below, which is
the most important section of this report.

## What shipped

| Bead | PR | What |
|------|----|------|
| `acetone-15e` | #199 | **`CALL … YIELD` aliasing, `YIELD *`, bidirectional relationship patterns** — the parser burn-down half of criterion 1. |
| `acetone-cbl.2` | #200 | **TCK read scenarios over built fixtures** with entity notation (nodes/relationships/paths, structural compare) and Cypher escape decoding. 1600 → 2142 passing. |
| `acetone-6gy` | #201 | **Label predicates in expression position** (`WHERE n:Label`, including self-loops). |
| `acetone-cxh` | #202 | **Pattern comprehensions** `[ (a)-[r]->(b) WHERE p \| expr ]`. |
| `acetone-2ck.4` | #203 | **Chained comparisons** desugared to conjunctions (`1 < 2 < 3`). |
| `acetone-2ck.5`, `.6` | #204 | **UNWIND streams into a following LIMIT**; bound relationship-list var-length pinning. |
| `acetone-6g5.7` | #205 | **Streaming import** (ADR-0062) — pull-based extraction, batched staging, workspace reset on failure. **Criterion 2.** |
| `acetone-6g5.3.3` | #206 | **IndexRange planner + executor and KeySeek point lookup.** |
| `acetone-0c7` | #207 | **Composite index seek acceleration.** |
| `acetone-ryg` | #208 | **Index-backed UNIQUE enforcement** and in-statement uniqueness. |
| `acetone-2ck.10` | #209 | **Lab seek-vs-scan measurements** at the larger envelope. **Criterion 3.** |
| `acetone-vgt` | #210 | **`merge_base` paint-down** — maximal-common ancestors in two linear walks. |
| `acetone-5a8` | #211 | **`fsck` anchor-completeness for commits** — the clean-now-gone-later class. |
| `acetone-6g5.10` | #212 | **Worktree-aware `gc`** and streaming `fsck` canonical rebuild. **Criterion 4.** |
| `acetone-2ck.8` | #213 | **Identity-keyed aggregate slots** — an aggregate in a skipped `CASE` branch no longer desyncs downstream aggregates. |
| `acetone-2ck.7` | #214 | **`NO_AGG` iteration bodies** — aggregates in comprehension/quantifier/reduce bodies are compile-time `InvalidAggregation`. |
| `acetone-2ck.9` | #215 | **`WITH … WHERE` applied after `SKIP`/`LIMIT`**, matching openCypher's sub-clause order. |
| `acetone-2ck.3` | #216 | **Undeclared-label advisory** for expression-position label predicates. |
| `acetone-2ck.11` | #217 | **`fsck` anchor-completeness for workspaces** (ADR-0063). |
| `acetone-cbl.2` | #218 | **Conformance statement published** with every residual parse rejection individually justified. **Criterion 1.** |
| `acetone-2ck.15` | #219 | **Both milestone-security blockers closed**; `KeySeek` wired into the shipped read path. |
| `acetone-2ck.14` | #221 | **`IndexRange` wired into the store-backed source** — range seeks reach the shipped read path, and decline when they would lose to a scan. |
| — | #220, #222 | The original report and deck, and a criterion-3 claim (#222) later **retracted** — part of the honest record; #223 (interim amendment) closed as superseded. |
| `acetone-2ck.2`, `acetone-7qw.9` | #224 | **The cost model** (ADR-0065): seeks spend a fixed fraction of the estimated scan cost and otherwise decline; **`WHERE` equality uses indexes**; hints are an ordered candidate list with fall-through. |
| `acetone-2ck.16` | #225 | **The lab measures the shipped path**: an identical unindexed twin repository, compared through `Session`, interleaved, with parity assertions. |
| `acetone-2ck.18` | #226 | **`declare-label --type`** — property types declarable through the CLI **and enforced** at write, declare and merge (ADR-0066, spec §2 amendment). |
| `acetone-2ck.17` | #227 | **Primary-key point lookups seek for string keys** when the key's type is declared: 240.9 ms → 0.25 ms on a 110,200-node graph. |
| `acetone-7qw.14` | #228 | **A conflicted workspace does not trust its own declared types** — seeks that would rely on one decline to a scan until the merge completes or aborts. |
| `acetone-7qw.16` | #229 | **Documented where a declared type is not trusted** (pre-enforcement repositories; `fsck` detects, the manual explains). |
| `acetone-2ck.20` | #230 | **Final-review fixes**: declare-time backfill checks stream instead of materialising the graph; a non-finite cardinality estimate is refused rather than saturating the seek budget. |

## Gate evidence — Phase 9 exit criteria

### Criterion 1 — TCK conformance, gaming-resistant ✅

**(a) No TCK scenario fails at parse; every residual rejection individually
listed and justified.** 3846/3897 queries parse. All 51 residual rejections are
enumerated in `docs/conformance.md` (the runner now emits them by name in
`parse.rejections`): **27 are correct rejections** — the scenario's own name
begins "Fail on…" and the TCK demands a syntax error; **22 are whole features
declined by design** (UNION ×12, existential subqueries ×10); and **2 are
genuine parser gaps** (`Set1 [3]`/`[4]`, a parenthesised expression as a `SET`
target — `acetone-2ck.12`).

**(b) Published pass rate ≥ 55%.** **2185/3897 = 56.07%, with zero failures**,
published in `docs/conformance.md`.

The statement is deliberately harder on itself than the number requires. It
discloses that **274 of the 2185 passes (12.5%) never execute the query** —
they are credited on a front-end rejection where the TCK expected a compile
error, and 27 of those verify only that *some* parse error occurred, not which.
It names the **three mechanisms that can convert a defect into a pass**
(unverified parse-rejection reason; structural identity-less entity comparison;
the write-query token escape that files front-end rejections of write queries as
deferrals — `acetone-s1j`), separately from the four that are genuinely
conservative. And it corrects the previous statement's account of the largest
gap: the 1138-scenario "deferred syntax" bucket is **mostly a bind-time decline
on unimplemented functions** (1114 of them parse fine), of which **temporal
types and functions are ~1025 — 90%** (`acetone-hi8`), where the old table named
`CALL {}`, `FOREACH` and `LOAD CSV`, none of which appear anywhere in the corpus.

### Criterion 2 — larger-than-memory import in bounded resident memory ✅

ADR-0062. Import is pull-based end to end: a `SourceExtractor` yields one record
at a time, batches of `DEFAULT_IMPORT_BATCH` (8192, `--batch-size` on the CLI)
are staged and flushed, and a single rewound file descriptor serves both the
hash pass and the parse pass. A 4×-source-size import holds **1.54× peak RSS**
rather than growing with the source.

Two honest scopings, both recorded when they were found rather than after:

- The bound is **unconditional only for UNIQUE-free imports**. With
  unique-constrained labels the tracker is O(nodes of those labels) — a compact
  interned structure with lazy seeding, but not constant. Stated in ADR-0062.
- **Wall-time is superlinear** even though memory is bounded, because every save
  re-anchors the whole chunk set (`acetone-taf`).

### Criterion 3 — index range and composite seek beat scan on the lab graph ✅

**The story of this criterion is the story of the phase's reopening**, so the
evidence is presented in the order it was learned.

**What was first published, and why it was withdrawn.** The original report
carried a table headlined 13.8× (range) and 27.6× (composite) at 440,800
nodes. The milestone security review found all of it was measured against the
**in-memory `GraphSnapshot`**, not the source `Session` uses — the shipped
`StoreBackedSource` implemented only equality seeks, so the product could not
even execute most of the table. Wiring `KeySeek` (#219) and `IndexRange`
(#221) into the shipped path then exposed what the in-memory source structurally
could not show: **a seek does one random point read per matching row, while
the scan it replaces reads sequentially.** Past roughly 1% selectivity the
"optimisation" *lost* — up to **37× slower than no index** for the range, 3.7×
for the composite. Declaring an index made ordinary queries dramatically
worse. The 13.8× and 27.6× cases were, on a real store, genuinely
scan-shaped: their speed-ups were artefacts of a source that pays nothing for
point reads.

**The fix (PR #224, ADR-0065): indexes are chosen by estimated cost.** The
planner samples the nodes map (mean fanout per level — a fixed two-dozen chunk
reads, memoised per query) to estimate what the scan would cost, and a seek
may spend 0.5% of it (`estimate/200`, floor 32) before declining to the scan.
Declining can only change plans, never rows: `None` means "cannot serve, scan
instead", and every served candidate set is re-filtered by the full predicate.
ADR-0065 records three rejected designs, one refuted by direct measurement.
Alongside it, **`WHERE` equality now attaches index hints** (`acetone-7qw.9`)
— previously only the inline `{p: 1}` form used an index — and hints are an
ordered candidate list with fall-through. Because a string seek is only sound
when a declared type rules out an encoding mismatch, this pulled in the CLI
work Greg directed: **property types declarable through the CLI and enforced**
(#226, ADR-0066), and **string primary-key seeks** (#227).

**The shipped-path evidence** (`lab/README.md`): the lab now builds an
identical unindexed **twin repository** (root-hash-verified — Invariant 1
makes root equality prove graph equality), and measures both through
`Session`, interleaved with the order swapped every other iteration, each case
asserting result parity on every run. At `--scale 50000` (110,200 nodes /
219,985 edges), best of 7:

| case | indexed | unindexed | ratio |
|------|--------:|----------:|------:|
| IndexRange, expiring tomorrow (0.27%) | 16.4 ms | 278.7 ms | **17.0×** |
| Composite seek, empty bucket | 0.4 ms | 246.2 ms | **643×** |
| IndexRange, expiring this week (1.9%) | 279.2 ms | 279.3 ms | 1.00× |
| IndexRange, expiring in 30 days (8.2%) | 282.1 ms | 281.7 ms | 1.00× |
| Composite seek, populated bucket (2.9%) | 247.9 ms | 249.4 ms | 1.01× |
| IndexSeek equality on `os` (20%) | 253.1 ms | 252.0 ms | 1.00× |
| WHERE equality on `os` (20%) | 286.4 ms | 285.8 ms | 1.00× |

And the point lookup, measured against the label scan it replaces on the same
repository (the twin ratio is pinned at 1.00× *by construction* here — both
twins declare the same key, so both plan the same `KeySeek`; the README
explains why):

| case | seek | full `Host` scan | ratio |
|------|-----:|-----------------:|------:|
| `MATCH (h:Host {hostname: 'host-49999'})` | 0.25 ms | 274.5 ms | **1104×** |

Until #227 that lookup took 240.9 ms — it scanned all 110,200 nodes to find
one node by its declared identity.

Read the table as two groups. **Selective seeks win large**: 17.0× at 0.27%
selectivity, and an absence proof at ~600× (quote that one as an order of
magnitude — it is scan time over fixed per-query overhead, and runs of the
same case measured 396–643×). **Unselective seeks decline to parity, and
that is the designed behaviour**: the 8.2% range and 2.9% composite are the
very cases published as 13.8× and 27.6× — on a real store they are
scan-shaped, and before ADR-0065 they lost by up to 37×. Now they cost the
scan ±1%. The residual is a **constant** (one index probe plus one
cardinality sample), so it is only negligible relative to a scan worth
avoiding: at `--scale 200` the same declining cases read 0.47–0.72×. Any of
these figures needs its scale attached. Run-to-run variance is real — the
final gate audit independently re-ran the suite at the same scale and got
14.35× / 594× / 0.97–1.01× / 843× for the four headline shapes, the same
story within noise. The `lab/README.md` best-of-7 table is the canonical
citation.

**The criterion's judgement call, dissolved rather than ruled on.** The
earlier interim finding — that the lab's own chosen range case declines
through `Session`, so the criterion turned on whether a decline "beats" a
scan — is gone: the lab's suite now spans selectivities, and the selective
cases win outright on the shipped path. What remains true is that the lab's
*original* two cases were scan-shaped all along; the phase's contribution is
that acetone now knows it at plan time.

### Criterion 4 — `fsck` and `merge_base` sub-quadratic on a multi-version repository ✅

Measured on this machine over linear histories built through the CLI:

| Commits | `fsck` |
|---------|--------|
| 200 | 0.193 s |
| 400 | 0.412 s |
| 800 | 1.125 s |

Doubling ratios 2.13 then 2.73 — per-doubling exponents 1.09 then 1.45, so the
endpoint fit is **≈N^1.27 and the exponent is climbing, not flat**. Comfortably
sub-quadratic across the measured range; the trend is worth re-checking at
larger N rather than averaging away. The
PR #211 reviewer measured ≈N^1.15 over N=100–800 on an all-distinct-manifest
worst case where the chunk-set cache never hits.

`merge_base`, measured across two divergent chains of N commits each:

| Commits per side | `diff main side` |
|------------------|------------------|
| 100 | 0.021 s |
| 200 | 0.022 s |
| 400 | 0.023 s |

These two tables, and the round-2 security magnitudes quoted below (10.6 GB,
6.4 GB, 0.08 s / 17 MB, 54.7 GB), were **measured in-session on this machine and
are not reproducible from the repository** — the scripts lived in a scratch
directory that is now gone. Every other figure in this report is checkable
against `docs/conformance.md`, `lab/README.md`, a bead close reason, a PR body,
an ADR or the source.

**Effectively flat** while the history grows fourfold — the linear paint-down
doing what it was written to do. PR #210's review additionally ran a
63,724-pair differential against the previous implementation with zero
divergences.

## Decisions taken

- **ADR-0062 — streaming import.** Pull-based extraction, batched staging,
  statement/stored UNIQUE layers, workspace reset on failure. Includes the
  honest memory scoping above.
- **ADR-0063 — workspace anchor coverage.** Self-anchoring is mandatory for
  every live version; borrowed coverage applies to exactly one shape (a
  superseded pre-ADR-0014 legacy shared ref that no writer can upgrade) and only
  from durable, gc-enumerable sources. Written *because* the first attempt at
  the rule was unsound — see below.
- **ADR-0064 — scanning is governed on its own budget.** `max_scanned_candidates`
  rather than borrowing the expansion budget, because borrowing it rejected
  ordinary nested-loop joins (a 20-row semi-join over 100k nodes). Written
  during PR #219's review, for the same reason ADR-0063 was.
- **ADR-0065 — indexes are chosen by estimated cost.** A seek spends a fixed
  fraction (0.5%) of the sampled scan estimate and otherwise declines.
  Records three rejected designs — an absolute cap, prolly-height tiering,
  and a bytes-proportional fraction that direct measurement refuted (36× the
  bytes moved break-even only ~2×; point reads are dominated by per-object
  overhead, not size).
- **ADR-0066 — a declared property type is a constraint, not an annotation.**
  Enforced at write, at declare (backfill, refusing a declaration the data
  contradicts) and at merge (a `WrongType` violation is data, per ADR-0007).
  **This is a spec §2 amendment** — previously only UNIQUE and existence were
  enforced, so unenforced types were conformant as written — and a behaviour
  change for library consumers: `Transaction::save`/`commit` can now fail
  where they previously succeeded. Flagged as a governing-document change;
  the alternative (ship the flag, enforce later) was rejected as the worst
  ordering — a seek trusting an unenforced declaration under-selects, which
  is a silent wrong answer on node identity.

## What review caught

This is the section worth reading. In four separate cases an adversarial
reviewer, executing rather than reasoning, found something that would have
shipped:

- **PR #217 took four rounds and found real data loss.** My first version
  reported a permanent, un-actionable error on any pre-ADR-0014 repository, with
  a gc claim that was demonstrably false. My second version — treating a chunk
  as safe if *any* tree anchored it — was driven by the reviewer to an actual
  `missing chunk` error: `fsck` clean → `acetone gc` → `git gc` → data gone,
  because the covering anchor was one `gc` itself deletes. My third version
  over-corrected and made the claim false again on the commonest legacy shape
  (fsck said 3 chunks doomed where `git gc` pruned 1). ADR-0063 exists because
  of that sequence.
- **PR #218 took three rounds on honesty, not correctness.** Every one of the
  14 published figures was right from the first commit; every substantive
  finding was in the prose interpreting them — a claim that all passes execute
  (274 do not), a claim that no limitation can inflate the count (three can),
  and a 1138-scenario bucket described by three features that appear in zero
  corpus files.
- **PR #206** would have silently dropped rows on cross-type key pins, and its
  hints were dead on the main path.
- **PR #208** admitted a signed-zero UNIQUE breach and falsely rejected NaN,
  because claim keys used CBOR rather than the memcomparable key encoding.

## Milestone security review

Security review ran **twice** this phase, and the coverage split matters for
anyone auditing it. The **milestone review** described in this section covered
the diff up to PR #218 (`848d0e9..`, 20 PRs, 49 files, ~7200 insertions); its
two blockers were fixed in PR #219. The reopened tail — PRs #219–#229, which
includes the cost model and the new CLI type-declaration input surface — was
covered by a **final phase-level review** (three fresh strongest-tier
reviewers: security, correctness, gate audit) run before this amended report;
its results are in *The final review* below. No part of the phase diff went
without a phase-level security pass, but no single pass covered all of it.

The milestone review returned **two blocker-class findings**, both since
fixed in PR #219 with regression tests:

1. **Exponential AST blowup** — the chained-comparison desugar clones interior
   operands, so nesting doubled the AST per level. A **268-byte query reached
   53.8 GB and 67 s inside `parse()`**, before any `Governor` exists and equally
   from any library consumer. Now bounded: the same bomb errors in 0.03 s at
   5.6 MB.
2. **Ungoverned per-row full scan** — a pattern comprehension with a fresh
   anchor re-materialised the whole node map per row at zero governor charge;
   `UNWIND range(1,1000000) AS i RETURN size([(x)-->(y) | 1])` hung
   indefinitely. Anchor candidates are now charged against a **dedicated
   budget** (`max_scanned_candidates`, ADR-0064), uniformly — no scan is exempt
   by ordinality. Two earlier designs were tried and rejected in review:
   charging work only (bounded but far too loose) and charging the expansion
   budget (tight, but it rejected ordinary nested-loop joins). The exploit now
   raises a typed `ResourceExceeded`.

**The security fix itself needed three rounds.** PR #219's own adversarial
review found that my first attempt at each blocker was incomplete, and every
one was demonstrated by execution rather than argued:

- The parse-time bound counted only *expression* nodes, so **pattern structure
  was a free multiplier** — a 100 KB query still reached 10.6 GB. It also
  applied per chain rather than cumulatively, so four sibling bombs composed to
  6.4 GB. Now the bound measures allocated structure (steps, labels, types) and
  accumulates across the parse: those queries error in 0.08 s at 17 MB.
- The scan charge closed `match_path` but **missed `pattern_exists`** — an
  anonymous start node in a pattern predicate had the identical unbounded
  per-row scan, and its edge traversal charged no hops either. Both now charge.
- `nodes_by_key` **introduced a wrong-answer regression**: a `Bytes`-keyed node
  compares equal to its string rendering at runtime but encodes differently, so
  a string pin served a probe set that *missed* it — `MATCH (n:Host {id:'x'})`
  and `MATCH (n:Host) WHERE n.id='x'` returned different rows. String pins now
  decline and let the scan answer.
- The probe cartesian's cap used `product()`, which **wraps in release**: a
  584-byte query with a 64-component key reached 54.7 GB. Now `checked_mul`,
  fixed on both the store and in-memory paths.
- Borrowing the expansion budget **false-positived on ordinary joins** — hence
  ADR-0064.

Three limits of the fixes are worth stating plainly, all measured by the
reviewer at the merged commit:

- **The scan budget under-counts.** `scan()` charges anchor *materialisation*,
  not the per-anchor `walk_steps`/`node_satisfies` work that follows — roughly
  40× under-count on the comprehension shape. So the headline exploit is
  bounded but still burns **787 s** before erroring on a 20k-node repository,
  against 18 s for the pattern-predicate shape at the same budget. Bounded is
  not the same as defended; `acetone-7qw.6` (memoise anchor scans) is the fix
  that makes these fast rather than merely finite.
- **`allocated_size` weights structure but not byte payloads.** A long string
  literal inside nested chains still amplifies ~780× — linear and refused in
  ≤ 212 ms, against the original exponential 106,000×, so the DoS class is gone,
  but the bound is not as tight as its name suggests (`acetone-7qw.8`).
- **`QueryLimits` and `ResourceLimit` gained a public field and variant** — see
  the API-freeze note under *Process notes* below.

Lesser findings are filed — five beads from the milestone review itself
(`acetone-7qw.2`–`.6`), plus two from PR #219's own review
(`acetone-7qw.7`, `acetone-7qw.8`) and the co-tenancy note (`acetone-42d`):
`acetone-7qw.2` (quadratic import
UNIQUE-violation path, executed and measured), `acetone-7qw.3` (schema-driven
panic), `acetone-7qw.4` (unbounded line length vs ADR-0062's promise),
`acetone-7qw.5` (the API-freeze gate cannot see enum-variant removals in
re-exported types), `acetone-7qw.6` (memoise anchor scans), `acetone-7qw.7`
(the scan budget's ~40×
under-count) and `acetone-7qw.8` (the byte-payload amplifier), and
`acetone-42d` (the co-tenancy ref-prefix note).

Defences that **held up under probing**, and are worth recording as much as the
findings: `prune_loose`'s safety floor survived every gc/worktree scenario the
reviewer built; ADR-0044 durability anchors survived a real
`git gc --prune=now --aggressive`; ref-name injection failed closed through both
git and gix; three million mutated queries produced no panics; and **no
dependency was added, bumped or re-featured all phase** (`cargo deny` and
`cargo audit` clean).

## The final review

Before this amended report was written, **three fresh strongest-tier
reviewers** ran in parallel over what the milestone review had not covered:
a security sweep of PRs #219–#229, a cross-PR correctness review of the
criterion-3 sequence (#224–#229), and an audit re-verifying every criterion's
evidence against current `main`. All three worked by execution, not argument.

**Security: GATE READY.** No blockers. Both milestone blocker fixes held
under re-attack (the 269-byte comparison bomb refuses in 7 ms at 3.2 MB; the
scan exploit terminates with its typed error). PR #228's conflicted-workspace
distrust was complete against every escape route tried (`resolve_all` both
ways, commit, abort). ADR-0066's enforcement resisted every bypass attempted,
including declare-and-write-bad in one transaction and
declare-plus-delete-the-offender (correctly accepted — nothing left to
violate — with resurrection then refused). Hostile `--type` input, 1 MB pins,
NaN/±∞/2⁵³ values: no panics, no terminal injection, no wrong answers. No
dependency changed all phase. Findings: one **High** — the scanned-candidate
budget is denominated for the in-memory executor, so the governed pathology
still burns ~13 minutes and 1.4 GB of *bounded* work on the store path before
its typed error (measured 30–47 µs per candidate; recorded on
`acetone-7qw.7`, whose business the re-denomination already was — and the
governor docstring that plausibly let this survive earlier review is
corrected in #230); one **Medium** and one **Low**, both fixed in PR #230
(the declare-time backfill check materialised the whole node map; a
non-finite cardinality estimate saturated the seek budget on a crafted
store); and one informational confirmation that the documented
pre-enforcement residual (`acetone-7qw.16`) is genuinely reachable from a
repository written by an older binary — `fsck` names it, the manual explains
it.

**Correctness: SOUND — no wrong-answer path found.** Fifty-eight
differential checks through `Session` on real repositories, hunting exactly
the class this phase's history warns about (declared type vs runtime type vs
key encoding): dual numeric encodings of the same key value, `Bytes`/string
key collisions, NFC/NFD unicode, signed zero, 2⁵³-boundary int/float,
parameter pins, conflicted-workspace states, mixed-encoding composite pins,
write-then-read within a session. Every seek spelling returned exactly the
scan's rows. The trust boundary (#227 trusts a declared key type; #228
withdraws trust in a conflicted workspace) is consistent across all four seek
forms, and the cost model provably changes plans only. Three Low findings:
the shape-closure asymmetry documented in #230 and filed as a design question
(`acetone-7qw.17` — declaring any type closes node-pattern map literals in
`CREATE`/`MATCH`/`MERGE` while `SET` stays open); numeric equality lossy
above 2⁵³ (`acetone-7qw.18`, pre-existing, seek path verified exactly
consistent with it); inline and `WHERE` pins never merge for composite seek
eligibility (`acetone-7qw.19`, plans only).

**Audit: the evidence reproduces.** The TCK re-run on current `main`
reconciled with `docs/conformance.md` item for item — 2185/3897, zero
failures, all 51 enumerated rejections matching by family. The lab re-run at
the README's stated scale confirmed the table's shape and absolute times
(variance noted in the criterion-3 section). All 29 beads closed during the
phase carry reviewer sign-off in their close reasons (one administrative
supersede-split aside, whose halves were reviewed under #219/#221). The
public-API freeze surfaces regenerate to empty diffs under the pinned
toolchain — which retires nothing about the ADR-0064 freeze question below,
since the gate is known-blind to field/variant changes inside re-exported
types. The audit's documentation punch-list (stale figures, the CHANGELOG
gap, follow-up homing, `conformance.md`'s date) is discharged by this
amendment and PR #230's bead work.

## Process notes and open risks

- **Review-tier deviation, disclosed.** Fable 5 capacity was exhausted partway
  through the phase. PR #217's rounds 2–4, PR #218, the milestone security
  review and PR #219 ran at **Opus** — the strongest tier the session could
  reach — rather than being downgraded silently or the gate being skipped. This
  is recorded on the beads as well as here. Under ADR-0009 the tier is defined
  relative to the strongest model the session has access to, so this is
  compliant, but it is a deviation from what previous phases did and Greg should
  know.
- **An API-freeze question for the boundary — since ruled.** ADR-0064 adds a
  public field to `QueryLimits` and a variant to `ResourceLimit`, both in
  STABILITY.md's curated frozen surface. That is source-breaking for exhaustive
  struct literals and `match`es inside a 0.3.x patch series whose policy is
  "additive only". **Greg's ruling at the boundary: cut 0.4**, and take
  `#[non_exhaustive]` on both types at the same minor boundary rather than
  waiting for 0.5.

  One correction to how this was first written up here. The claim that "the CI
  freeze gate cannot see either change" was **wrong**, and cutting 0.4 proved
  it: `crates/acetone-cypher/public-api.txt` is signature-tracked and records
  `max_scanned_candidates: u64` and `ScannedCandidates` by name, so that gate
  *did* cover ADR-0064. The `acetone-7qw.5` blind spot is real but narrower —
  it covers types re-exported from `acetone-graph`, `acetone-model` and
  `acetone-store`, which appear in the core snapshot by name only. The
  demonstration is `GraphError`: making it `#[non_exhaustive]` for 0.4
  re-blessed to an **empty diff** despite being source-breaking. STABILITY.md
  now states the covered and uncovered sets explicitly.
- **The review gate leaves no externally auditable trace.** Every PR this phase
  was reviewed by a fresh subagent with no implementation context, but that
  review lands in bead close reasons and PR descriptions — not as a GitHub
  review. An outside checker, or a future agent, cannot reconstruct what a
  reviewer actually ran or found; they have only the implementer's account of
  it. That is a durable property of how the gate is operated rather than a fact
  about any one PR, and it is load-bearing here: parts of the performance table
  above reach this report through that channel and nothing else.
- **PR #218 carried a code change** (`tck/src/classify.rs`) alongside the
  document, so it took the **full adversarial path** rather than the lighter
  docs review; the reviewer confirmed count-neutrality empirically across three
  runner runs.
- **Follow-ups crossing the boundary**, each with its reason: `acetone-2ck.12`
  (SET parenthesised target — surfaced *by* the conformance enumeration at the
  boundary itself), `acetone-s1j`
  (write-query escape — pre-existing, now disclosed), `acetone-hi8` (temporal
  support — a whole feature family, not a defect), `acetone-1qj` (binder
  over-accept), `acetone-taf` (import wall-time), and the security and
  quality beads under `acetone-7qw`. The reopened tail resolved two of the
  original list in-phase after all — `acetone-2ck.2` (the cost model shipped
  in #224, retiring both the absolute-cap limitation and the equality-seek
  cliff it carried) and `acetone-7qw.9` (`WHERE` indexing, same PR) — and
  added its own: `acetone-7qw.10` (cost the hint candidates rather than
  first-serve — measured 65× off the best plan in one band), `acetone-7qw.11`
  (the estimator reads 1.96×-high/0.02×-low and bimodal on skewed builds),
  `acetone-7qw.12`/`.13`/`.15` (review minors), `acetone-7qw.16` (the
  pre-enforcement declared-type residual — its durable fix is format-coupled,
  so it is dep-linked to a new format-boundary gate bead, `acetone-qjzy`),
  and from the final review `acetone-7qw.17`/`.18`/`.19` (above). Each is a
  real future unit of work with its measurements attached; none blocks the
  ratified criteria. **`acetone-7qw` has been reopened** to own them — it had
  been closed at the 0.3.1 boundary, which left Phase 9-surfaced follow-ups
  floating under a closed epic against ADR-0054. Note that
  `acetone-2ck` still carries **16 open beads** that predate this phase — they
  were parented there during the Phase 8 backlog triage, not surfaced by Phase
  9 — plus the gate bead itself. Deciding whether those move or stay is Greg's,
  not something this phase should have quietly re-filed.
- **One reviewer was replaced mid-review.** PR #226's first assigned reviewer
  went idle four times without delivering; a fresh reviewer was dispatched
  with the same brief and no partial context, and its review is the one of
  record (noted on the bead).
- **Behaviour changes worth knowing about**: `WITH … WHERE` now filters *after*
  `SKIP`/`LIMIT`, which changes results for queries combining them (conformant,
  and the TCK does not pin it); `ORDER BY` keys are now evaluated on rows the
  `WHERE` will discard, so a sort-key error can surface where it previously did
  not; and aggregates inside comprehension/quantifier/reduce bodies are now
  compile-time errors rather than silently wrong answers.

## Gate readiness

All four criteria have evidence; the final review's audit independently
re-verified criteria 1 and 3 by execution on current `main` (the TCK
reconciled item-for-item; the lab re-run at the published scale) and
confirmed criteria 2 and 4's mechanisms intact via the green suites —
criterion 4's original tables remain in-session measurements, as disclosed
in its section. Both milestone
security blockers are closed and their fixes held under fresh attack; the
final security pass over the reopened tail is GATE READY. The criterion-3
judgement call the interim reports carried is dissolved: selective seeks win
outright on the shipped path (17×/643×/1104×), unselective seeks decline to
parity by design, and the equality-seek cliff that shipped on `main` since
PR #123 — 18× slower than no index — is **fixed** by the cost model rather
than carried as an open risk (measured 1.04–1.23× post-#224, selective gains
intact).

**Boundary outcome (2026-07-31).** Greg ran the sprint demo — conformance
live, both security blocker fixes, the lab's twin-repository criterion-3
measurement through `Session`, declared-type enforcement, and `fsck` naming
the pre-enforcement residual — then **ratified ADR-0062–0066 and accepted the
gate evidence**. The ADR status lines record it.

What remains for Greg, rather than for an agent:

1. **The API-freeze question** ADR-0064 raised — a public field on
   `QueryLimits` and a variant on `ResourceLimit` inside a 0.3.x "additive
   only" series, both invisible to the CI freeze gate (`acetone-7qw.5`);
   `#[non_exhaustive]` or a minor bump. (`acetone-fht` is the precedent on
   the books, but it is scoped to `GraphError` only — these two types have
   no equivalent bead yet.) Not blocking the gate; it decides what the next
   version number is.
2. **Close the gate bead** `acetone-2ck.1` — the evidence is accepted, and
   the close is the human act the protocol reserves. Rule at the same time
   on whether the 16 pre-phase beads under `acetone-2ck` move or stay.
