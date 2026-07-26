# Phase 9 report — at scale and in conformance

*Epic `acetone-2ck` · base `main @ 848d0e9` (post-0.3.1) · this report covers PRs #199–#221*

Phase 9 is the phase where acetone stops being demonstrably correct on small
inputs and starts being **measurably correct at size**. Two things had to
happen together: the query engine had to stop declining large parts of
openCypher (the conformance half), and the storage and verification paths had
to stop assuming everything fits in memory or in one pass (the scale half).

The headline numbers: **TCK conformance moved from 1602/3897 (41.1%) with 50
failures to 2185/3897 (56.07%) with zero failures**; import of a source larger
than memory completes in bounded resident memory; index seeks beat scans by
4.5×–10⁵× **on the lab's in-memory source**, though
not on the shipped path for the lab's own populated cases (see criterion 3);
and `fsck` and `merge_base` are verified
sub-quadratic on multi-version repositories.

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

### Criterion 3 — index range and composite seek beat scan on the lab graph ⚠️

Measured on the lab graph at 440,800 nodes (`lab/README.md`):

| Seek | Hinted | Scan | Speed-up |
|------|--------|------|----------|
| IndexRange (`cert_not_after < 30`) | 24.4 ms | 337.8 ms | **13.8×** |
| Composite seek (populated bucket) | 7.7 ms | 213.1 ms | **27.6×** |
| Composite seek (empty bucket) | 0.001 ms | 208.2 ms | ~10⁵× |
| KeySeek (primary key) | 0.002 ms | 201.0 ms | ~10⁵× |
| IndexSeek equality (`host_os`) | 53.0 ms | 236.2 ms | **4.5×** |

Each case carries a parity assertion, so a "speed-up" that returned different
rows would fail rather than flatter.

**Where these numbers came from, and what has since changed.** The milestone
security review found that `StoreBackedSource` — the source `Session` actually
uses — implemented only `nodes_by_index`, and that the lab builds a
`GraphSnapshot` for *every* case, so **all the rows above were measured against
the in-memory source, not the shipped read path**. `KeySeek` was wired in
PR #219 and `IndexRange` in PR #221, so both now reach the product. **The lab
still measures the in-memory source and `lab/README.md` still does not say so**
— that disclosure was on `acetone-2ck.14`, which closed with the re-measurement
half undone, so it is re-homed to `acetone-2ck.2` rather than left on a closed
bead.

Wiring the range seek then exposed something the lab could never show, because
the in-memory source does not pay for it: **a seek does one random point read
per matching row, while the scan it replaces reads the nodes map sequentially.**
Past roughly 2.5% selectivity the "optimisation" was *up to 37× slower than no
index at all*. Declaring an index made ordinary queries dramatically worse.

The fix is to decline rather than to optimise: past 1024 candidates the source
returns `None` — the `GraphSource` contract's "cannot serve, scan instead" —
before paying for a point read in that family. (The budget is checked per
family, so a numeric range whose int half fits under the cap does those reads
before the float half can trip the decline.) Measured by PR #221's reviewer on
a 50,000-node graph with three binaries side
by side (always-scan `main`, always-seek, and the shipped fix). These are that
reviewer's own run, not the four-row table in PR #221's description, which used
a different data shape and a 150.4 ms scan baseline:

| rows | always scan | always seek | shipped |
|---:|---:|---:|---:|
| 51 | 105.4 ms | 8.2 ms | **8.2 ms** |
| 494 | 105.0 ms | 53.2 ms | **54.2 ms** |
| 2,030 | 107.6 ms | 208.4 ms | **100.3 ms** |
| 24,973 | 117.9 ms | 2570.2 ms | **111.9 ms** |

No regime is slower than the scan, and the selective win is intact.

**And this is where the criterion's judgement call actually sits — for both of
its named seek kinds, not just one.** An earlier version of this section
caveated only the range seek. Measuring the composite case at the boundary
showed that understated the problem.

*The range seek.* The lab's own range case would decline at the shipped cap:
`cert_not_after` is `i % 365` and the lab runs at 200,000 certificates, so
`cert_not_after < 30` matches roughly **16,000 rows — sixteen times
`MAX_RANGE_CANDIDATES`**. Through `Session` that query declines and label-scans,
giving **1.0×, not the 13.8×** in the table above.

*The composite seek.* The lab's composite bucket is ~1/35 of hosts, i.e. **2.9%
selectivity**. Reproduced at exactly that ratio on a 50,000-node repository
(1,429 of 50,000 matching rows), comparing an indexed repository against an
identical unindexed one, five runs each:

| case, 2.9% selectivity | indexed | unindexed | |
|---|---:|---:|---|
| populated bucket, loose objects | 324.9 ms | 88.2 ms | **3.7× slower** |
| populated bucket, after `gc` | 112.4 ms | 77.2 ms | **1.5× slower** |
| empty bucket, loose | 29.1 ms | 76.9 ms | 2.6× *faster* |

So the composite seek does not decline — the equality path has no cap, which is
the pre-existing cliff described below — it fires and **loses**, where the lab
reports 27.6× faster. The empty-bucket case is the one row of that table that
survives contact with the store: proving absence really is cheap, because there
are no candidates to fetch.

*Why both.* A seek on the real store does **one random point read per matching
row**; the scan reads the nodes map sequentially. In the lab's in-memory source
a "seek" is a vector lookup that costs nothing, so any selectivity looks like a
win. On the store, a seek wins only while it is selective — and the lab's own
parameters, at 2.9% and 8.2%, are not. Nor would `acetone-2ck.2`'s
cardinality-informed threshold rescue them: break-even is roughly 2% on loose
objects and 8% packed, so a perfect cost model declines the range case and would
have to decline the populated composite case too. **These are genuinely
scan-shaped queries; the lab's speed-ups are artefacts of a source that pays
nothing for point reads.**

*What is genuinely delivered on the shipped path*, all measured through the CLI:
a **selective** range (250 of 50,000 rows) at 2.8×; **absence proofs** on an
empty bucket at 2.6–3.3×; and **point lookups** by primary key. What is not
delivered is a speed-up on either of the lab's two populated cases.

*A third thing the measurement turned up*: the equality hint attaches only for
an inline pattern pin. `MATCH (n:H {b: 3})` uses the index (324.9 ms indexed vs
88.2 ms unindexed — it fires and loses); `MATCH (n:H) WHERE n.b = 3` is
identical indexed and unindexed (104.7 vs 104.4 ms), i.e. **no seek is used at
all** for the form most people write. Range predicates in `WHERE` *do* attach a
hint, so this is specific to equality. Filed as `acetone-7qw.9`, with the note
that closing it before `acetone-2ck.2` would expose *more* queries to the cliff
rather than help.

*Greg's ruling, at the boundary.* Neither of the two options an earlier draft
of this section offered is the point. The real problems are that **acetone
selects an index seek when it should not — the cost model needs refining — and
that `WHERE` does not use indexes at all.** Re-parameterising the lab to
demonstrate wins at reasonable selectivities is needed, but is **secondary to
having useful beneficial indexing**.

That reframes what this criterion has actually shown. Both seek kinds are now
reachable through `Session`, which they were not when this report was first
written, and each beats a scan on selective inputs — but what the boundary
measurements really established is that **a declared index is not yet reliably
beneficial**: it can make a query 1.5–3.7× slower on the lab's own composite
ratio, 18× slower on a 20%-selectivity equality case, and it does not engage at
all for the `WHERE` form most people write. The lab's parameters were not
exercising the regime where seeks help, but that is the smaller finding; the
larger one is that the seek/scan decision is being made without a cost model.

The work follows from that, in order:

1. **`acetone-2ck.2` (now P1) — the cost model.** Selectivity-estimated
   seek-vs-scan for *both* the equality/composite and range paths, replacing
   PR #221's absolute `MAX_RANGE_CANDIDATES` stopgap, so an index helps whenever
   it can and never hurts. `MapRoot.height` is a zero-cost coarse cardinality
   proxy already in the manifest.
2. **`acetone-7qw.9` (now P1) — `WHERE` must use indexes.** Sequenced after or
   with the cost model: closing it first would expose *more* queries to the
   cliff rather than help.
3. **Lab re-parameterisation and re-measurement through `Session`** — secondary,
   and worth more once the numbers describe a mechanism that reliably wins.

An earlier draft of this report claimed the criterion-3 question had
disappeared, then claimed it applied only to ranges. Both were wrong. The
accurate statement is above, and the criterion's disposition is Greg's when he
closes `acetone-2ck.1`.

**Two further open risks fall out of this.** First, the
cap is absolute where break-even scales with label cardinality (measured: ~1,000
rows at 50k nodes loose, ~4,000 packed, ~7,000 at 400k), so it is progressively
conservative on larger graphs. Second — and this one ships on `main` today —
**the equality-seek path has the same cliff and no cap**: an unselective
`IndexSeek` measured **1248 ms indexed against 67 ms unindexed, 18× slower**
(independently reproduced by the amendment's reviewer at 13.7× on a leaner node
shape). It arrived with PR #123 (`acetone-cbl.11`, lazy store-backed
`IndexSeek`), not with this phase — an earlier draft of this paragraph blamed
PR #206, which is *itself Phase 9 work* and would have made the sentence
self-contradictory. Both are recorded with their measurements on `acetone-2ck.2`
(costed planning seeds), which the roadmap names in this phase's paragraph and
which did not ship.

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

A dedicated security-focused review ran over the whole phase diff
(`848d0e9..main`, 19 PRs, 48 files, ~7000 insertions) before this report. It
returned **two blocker-class findings**, both since fixed in PR #219 with
regression tests:

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

Lesser findings are filed — six from the milestone review itself
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

## Process notes and open risks

- **Review-tier deviation, disclosed.** Fable 5 capacity was exhausted partway
  through the phase. PR #217's rounds 2–4, PR #218, the milestone security
  review and PR #219 ran at **Opus** — the strongest tier the session could
  reach — rather than being downgraded silently or the gate being skipped. This
  is recorded on the beads as well as here. Under ADR-0009 the tier is defined
  relative to the strongest model the session has access to, so this is
  compliant, but it is a deviation from what previous phases did and Greg should
  know.
- **An API-freeze question for the boundary.** ADR-0064 adds a public field to
  `QueryLimits` and a variant to `ResourceLimit`, both in STABILITY.md's curated
  frozen surface. That is source-breaking for exhaustive struct literals and
  `match`es inside a 0.3.x patch series whose policy is "additive only". Worse,
  `crates/acetone-core/public-api.txt` records only the re-export line, so **the
  CI freeze gate cannot see either change** — the same blind spot as
  `acetone-7qw.5`. The options are `#[non_exhaustive]` on both types, or a minor
  version bump; either way it is Greg's ruling, not an agent's.
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
  boundary itself), `acetone-2ck.2` (costed planning seeds — carrying the
  absolute-cap limitation, the pre-existing equality-seek cliff, and the lab
  re-measurement disclosure inherited from `acetone-2ck.14`, all with
  measurements), `acetone-s1j`
  (write-query escape — pre-existing, now disclosed), `acetone-hi8` (temporal
  support — a whole feature family, not a defect), `acetone-1qj` (binder
  over-accept), `acetone-taf` (import wall-time), and the seven security beads
  above. All of these are now re-homed to `acetone-7qw` (the 0.3.x quality and
  security epic) rather than left under the closed phase — though `acetone-7qw`
  is itself closed, so this is a move to a better-named home rather than to an
  open one. Note that
  `acetone-2ck` still carries **16 open beads** that predate this phase — they
  were parented there during the Phase 8 backlog triage, not surfaced by Phase
  9 — plus the gate bead itself. Deciding whether those move or stay is Greg's,
  not something this phase should have quietly re-filed.
- **Behaviour changes worth knowing about**: `WITH … WHERE` now filters *after*
  `SKIP`/`LIMIT`, which changes results for queries combining them (conformant,
  and the TCK does not pin it); `ORDER BY` keys are now evaluated on rows the
  `WHERE` will discard, so a sort-key error can surface where it previously did
  not; and aggregates inside comprehension/quantifier/reduce bodies are now
  compile-time errors rather than silently wrong answers.

## Gate readiness

Criteria 1, 2 and 4 are met. Both security blockers are closed. **Criterion 3 is
the one that needs a ruling**, and the honest summary is stronger than an
earlier draft of this report gave: its *reachability* shortfall was closed in
PR #221 rather than carried across the boundary, but on the lab's own two
populated cases the seeks do not beat a scan through `Session` — the range case
declines to a scan (1.0×) and the composite case is *slower* than no index
(1.5–3.7×). Both are genuinely scan-shaped at the lab's parameters, so no cost
model would change it. What the phase does deliver on the shipped path is
selective ranges (2.8×), absence proofs (2.6–3.3×) and point lookups. The
section above sets out the measurements and the two ways forward: rule that
reachability plus selective-case wins satisfies the criterion, or re-parameterise
the lab cases and re-run.

Three things therefore remain for Greg rather than for an agent: **whether
criterion 3 is satisfied** on that reading; the **API-freeze question** ADR-0064
raises; and the **pre-existing equality-seek cliff** (18× slower than
no index on an unselective seek) that ships on `main` today, arrived with
PR #123 rather than this phase, and is now measured and recorded on
`acetone-2ck.2` with a demonstrated remedy.
