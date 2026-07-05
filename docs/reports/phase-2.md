# Phase 2 report — the openCypher read path

*2026-07-05 · Epic acetone-yzc · For Greg's review at the Phase 2 boundary.
The exit bead (acetone-yzc.9, Gate C) is open and is yours to close; its
working deliverables are merged and the gate evidence is below.*

## What shipped

| Bead | Deliverable | PR |
|---|---|---|
| acetone-yzc.1 | **Gate B decision** (ADR-0013): build the Cypher parser, don't adopt decypher — settled with a two-parser spike against a representative query set | #29 |
| acetone-yzc.2 | `acetone-cypher` parser: spanned lexer + recursive-descent/Pratt parser for spec §5.1 Level R plus `AT <ref>` and `CALL … YIELD`; no-panic + bounded-recursion guarantees, fuzz-tested | #30 |
| acetone-8ng | CLI broken-pipe fix: `outln!` treats a closed stdout as clean exit (resolved a recurring e2e CI flake) | #31 |
| acetone-yzc.3 | openCypher TCK harness: vendored corpus (pinned `677cbafa`, 3,897 scenarios), honest classification, conformance report published as a CI artefact per commit | #32 |
| acetone-yzc.4 | Binder: name resolution, scoping, aggregation validation, entity-kind tracking, function/procedure registries, Strict/Lenient modes, planner index hints | #33 |
| acetone-yzc.5 | Executor: openCypher value semantics (three comparison regimes), expression evaluation, pattern matching, the clause pipeline over a provider-pluggable `GraphSource`; TCK result verification | #34 |
| acetone-yzc.6 | CLI `query` (`--at`, table/JSON/CSV) and interactive `shell` REPL; the acetone-graph→executor `GraphSnapshot` adapter | #35 |
| acetone-15e | Parser burn-down: list-predicate quantifiers (`all`/`any`/`none`/`single`) and `reduce` — the dominant TCK parser gap (652 of 704 failures) | #36 |
| acetone-yzc.7 | `AT <ref>` clause-group time travel: a `VersionResolver` seam; distinct `MATCH … AT` clauses may address distinct versions | #37 |
| acetone-yzc.8 | Lab asset graph (deterministic 50k-host registry) + realistic registry queries; graph-adapter indexing and key-property queryability, both surfaced by the full-scale run | #38 |

Every PR passed the adversarial review gate with explicit reviewer
sign-off; every non-trivial fix commit was re-reviewed. Several PRs took
multiple rounds (#30: three — two DoS blockers then a disambiguation
regression; #32, #33, #35, #38: two each). #34 signed off on the first
pass with two minor code fixes folded in.

## Gate evidence against the roadmap's exit criteria

The Phase 2 exit criteria are: (1) a published TCK read pass rate at or
above a chosen MVP bar with a tracked climb; (2) a lab asset graph
queryable with realistic registry queries at interactive latency; (3)
`AT <ref>` time travel over the Phase 1 commit graph.

### 1. TCK conformance — published, and the honest MVP line (Gate C is yours)

The harness runs all **3,897** TCK scenarios (upstream `677cbafa`) on
every commit and publishes the report as a CI artefact. Current numbers:

- **1,372 passed (35.2% of the whole corpus)**, 52 failed, 2,473 unsupported.
- Of the **1,424 scenarios the front end fully processes** (parse → bind →
  execute against a fixture the executor can build), **1,372 pass —
  96.3%**, with **zero execution result mismatches**. Every passing
  scenario's result table is verified cell-by-cell against the TCK's
  expected output; every failing one is a specific, named parser gap.
- The classification is deliberately conservative (documented in
  `tck/src/classify.rs`): a scenario counts as Passed only when verified
  end to end; deferred-syntax rejections are never credited.

Why "unsupported" is large and honest: of the 2,473, most need something
out of Phase 2's scope, not a read feature we failed —
**1,423 deferred syntax** (Level W write clauses `CREATE`/`MERGE`/`SET`/
`DELETE`, which arrive in Phase 3, plus the §5.1 explicit deferrals), and
**752 need the executor's fixture path**: the majority are read-feature
scenarios whose graph is built by `CREATE` setup — untestable until the
Phase 3 write path exists, not a read-path failure. **298** need static
type-checking (a compile-time error class acetone does not model).

The roadmap suggested "60% of read features" as the MVP line. Framed as
"of the read scenarios we can execute and verify, how many are correct,"
the answer is **96.3%**. Framed as "the whole corpus," 35.2%, with the
gap dominated by Phase-3 write dependencies. **Gate C — the choice of the
published bar for MVP honesty — is your call**; the evidence is the two
framings above and the per-area breakdown in the CI artefact.

The remaining **52 parser failures** are tracked (acetone-15e) and break
down into small classes: `CALL … YIELD col AS alias` on stub procedures
(~22; these become Unsupported, not passes, when parsed), pattern
comprehensions `[(n)-->() | e]` (~16, the largest real feature),
bidirectional `<-[:R]->` (4), `UNION` (4), `i64::MIN` literals (3), misc
(3). The roadmap's "tracked climb thereafter" is this bead.

### 2. Lab asset graph at interactive latency — met

`acetone-lab` (PR #38) generates a deterministic security-asset registry
(hosts/software/suppliers/certificates with a declared schema and a
secondary index). `cargo run --release -p acetone-lab --bin lab -- <repo>
--scale 50000` builds **110,200 nodes / 219,985 edges** (~the roadmap's
50k/200k target; `scale` = host count) and runs the registry query suite.
After the graph-adapter indexing fix (below), latencies at 50k/220k:

| query | rows | latency |
|---|--:|--:|
| certificate expiry sweep | 100 | ~220 ms |
| orphaned software | 500 | ~160 ms |
| supply-chain blast radius (var-length deps) | 1 (count) | ~130 ms |
| hosts by OS (indexed property) | 1 (count) | ~35 ms |
| critical hosts running a DE-supplier package | 1 (count) | ~1.1 s |

All interactive except the heaviest full-graph two-hop join (~1.1 s,
honestly flagged; a candidate for the streaming/costed-planner work
beyond 0.1). Per-query correctness is asserted at a small deterministic
scale in `lab/tests/registry.rs`.

**The lab graph earned its place by exposing two real bugs the smaller
tests could not.** (a) The first full-scale run took **30 s / 21 s /
147 s** on the multi-hop queries — the executor's graph adapter scanned
the entire edge set on every expansion (O(nodes·edges) over a MATCH). The
adapter now builds id/label/adjacency indexes; the reviewer proved
result-equivalence by a TCK A/B (identical 1,372, zero mismatches). (b)
The var-length query silently matched nothing because a node's **key
property** (its identity, spec §2/§3) was not filterable or returnable —
a latent bug that also affected the already-merged CLI. Fixed at the root:
the adapter re-exposes key values under their schema-declared names.

### 3. `AT <ref>` time travel — met

Whole-query `--at <ref>` (PR #35) and clause-group `MATCH … AT <ref>`
(PR #37) both work over the Phase 1 commit graph. Verified: a query at an
old commit sees old data; distinct `AT` clauses in one query address
distinct versions; unresolvable refs give a clean error, never a panic.
The reviewer's headline check confirmed cross-version entity identity is
**sound** — a direct consequence of natural-key identity (Load-Bearing
Invariant #3): an `AT`-clause node's values are a version snapshot, but
its identity is version-stable, so re-anchoring in a later base clause
walks base topology deterministically.

## Decision gate: Gate B (mid-phase, decided by ADR)

**Gate B — parser adoption** is a mid-phase gate, so it was decided by
ADR (ADR-0013) so work could proceed, and is flagged here for your
retrospective review. The decision: **build the parser in
`acetone-cypher`, do not adopt the `decypher` crate.** Evidence
(`spikes/cypher-parser-spike`): decypher 0.2.0-alpha.6 parsed 26/31 of a
representative valid-query set (failing pattern predicates and list
comprehensions, and unable to express `AT <ref>` without a fork); a
hand-rolled slice parsed 31/31. The parse boundary is kept narrow so the
choice is revisitable for GQL drift. This mirrors the Gate A
prollytree precedent — adopt-vs-build settled by running-code evidence.

## ADRs taken

- **ADR-0013** — Gate B: build the Cypher parser, don't adopt decypher.

(No on-disk format changed in Phase 2 — the read path is a pure consumer
of the Phase 1 storage layer, so `FORMAT_VERSION` is untouched and no
format-class ADR was needed.)

## Review findings summary

The mandatory adversarial gate did its job — reviewers caught real
defects that the implementation and its own tests had missed:

- **PR #30 (parser)** — two untrusted-input **DoS blockers**: exponential
  backtracking in the pattern-vs-expression disambiguation (a 176-byte
  query took 10 s), and an AST-depth guard that let a crafted query parse
  `Ok` then stack-overflow on `Drop`. Both fixed (linear token-scan
  disambiguation; iterative `Drop` + a total-AST-depth bound) and
  reviewer-reproduced as resolved. A follow-on disambiguation regression
  (`(1) - -2` false-rejected) was then caught and fixed in the same PR.
- **PR #34 (executor)** — the reviewer found `^` was implemented
  right-associative; the TCK pins it left-associative (a reversal of what
  both the Gate B spike and the PR #30 review had asserted). Fixed, with
  three other TCK-pinned front-end bugs execution surfaced (IN/STARTS WITH
  precedence, NaN comparisons, parenthesised column names).
- **PR #35 (CLI query)** — the reviewer found table/CSV output leaked raw
  terminal-escape sequences from hostile-clone labels and property
  values, regressing PR #25's sanitisation bar. Fixed (route graph strings
  through `sanitise_line`), mutation-verified.
- **PR #38 (lab graph)** — the reviewer found the flagship var-length
  query was silently vacuous (filtering on an unqueryable key property),
  surfacing the key-property root-cause bug fixed there.

Every finding was fixed or explicitly deferred with a tracked bead; no PR
merged over an unresolved reviewer objection.

## Milestone security review

A dedicated security-focused review (fresh subagent, strongest tier) ran
over the whole Phase 2 diff — untrusted-input handling, terminal-escape
injection, path/ref injection, panics on hostile data, dependency risk —
with 30+ self-constructed adversarial probes and `cargo deny check`.

**Result: Phase 2 is free of blocker-class security findings — the gate is
security-ready.** Verified clean, with evidence:

- **The parser (the primary untrusted-input surface) is robustly bounded
  and panic-free.** The `MAX_DEPTH = 64` parse guard and the
  `MAX_AST_DEPTH = 256` iterative post-parse check hold against 100k-deep
  nesting, 200k-term operator chains, 5 MB literals, adversarial UTF-8
  (RTL/BOM/NUL) and the pattern-vs-expression disambiguation area — all
  controlled errors on both an 8 MiB and a 2 MiB stack. No `unsafe` in
  the Phase 2 code.
- **Arithmetic is fully checked** — overflow, `/0`, `%0`, `i64::MIN / -1`,
  index/slice bounds all return errors, not panics.
- **Ref/path handling cannot escape the repository or spawn processes** —
  refspecs (`AT`, `--at`, `:checkout`) are validated through gix's
  `FullName` (rejecting `..`, absolute paths, metacharacters); `gix` is
  built with no `command`/transport features; nothing in the REPL shells
  out or evals.
- **Dependencies are clean** (`cargo deny` green); the new deps
  (`gherkin`, `serde`) are confined to the TCK harness crate; the spike's
  `decypher` is genuinely workspace-excluded (absent from `Cargo.lock`).

**Findings triaged (none blocker-class):**

- **MAJOR — no query-engine resource governor** (two exploitable cases):
  unbounded var-length `MATCH (a)-[*]->(b)` did not finish in 20 s on a
  9-node/72-edge complete graph, and `RETURN range(0, 1e10)` was
  OOM-killed. For the CLI (operator runs their own queries) this is
  self-inflicted; the real risk is acetone's stated library-embedding of
  untrusted queries. **Filed as acetone-iq6** (a configurable
  time/row/hop/list-size governor; absorbs acetone-18z). This is the top
  hardening priority and is recorded as an **open risk**, not a Phase 2
  blocker.
- **MAJOR — shell `:log` bypassed terminal-escape sanitisation** (a
  regression of PR #25's bar): **fixed** in PR #39, mutation-covered.
- **MINOR — JSON output left DEL/C1 controls unescaped** while table/CSV
  did not: **fixed** in PR #39 (ESC was already escaped, so classic CSI
  injection was blocked; this closes the residual gap).
- **NIT — unbounded recursion over nested stored values** (defence in
  depth, bounded upstream by CBOR decode limits): **filed as
  acetone-5xp** for the format-freeze hardening pass.

The two cheap terminal-escape items shipped in PR #39 with mutation-tested
regression coverage; the resource-governor risk is tracked and disclosed.

## Open risks and follow-up work

Tracked beads carried out of Phase 2 (none blocker-class):

- **acetone-iq6** *(from the security review, top priority)* — the query
  engine has no resource governor: unbounded var-length expansion and
  `range()`/eager-list materialisation can drive unbounded CPU/memory
  from a single untrusted query. Self-inflicted for the CLI; the risk is
  the library-embedding-of-untrusted-queries case. A configurable
  time/row/hop/list-size cap; absorbs acetone-18z.
- **acetone-15e** — the 52 remaining TCK parser gaps (the "tracked climb"):
  pattern comprehensions, `YIELD … AS`, bidirectional rels, `UNION`,
  `i64::MIN` literals.
- **acetone-1qj** — binder refinements: ORDER BY / aggregation re-scoping
  over-accepts (four TCK-pinned classes deferred, not credited) and the
  Strict property-access narrowing.
- **acetone-18z** — the executor's var-length expansion is exponential on
  dense graphs (correct openCypher trail semantics, bounded by edge count
  so no stack overflow); needs an expansion/result cap before it meets
  large untrusted graphs.
- **acetone-75o** — AT refinements: per-ref snapshot caching; AT-version
  schema binding.
- **acetone-3w5** — a narrow, documented pattern-vs-arithmetic
  disambiguation divergence (matches the Neo4j reading).
- **acetone-5xp** *(from the security review)* — unbounded recursion over
  deeply-nested stored list/map values (defence in depth; bounded upstream
  by CBOR decode limits, for the format-freeze hardening pass).
- **acetone-0ds** — the pre-existing bidi/zero-width unicode spoofing
  residual (control chars are neutralised; visual-spoofing homoglyphs are
  not, tracked from PR #25).

## Decisions queued for your ruling at the boundary

1. **Gate C** — the published TCK bar for MVP honesty (see §1). The
   evidence supports "96.3% of executable read scenarios" as the honest
   line; the whole-corpus 35.2% is dominated by Phase-3 write
   dependencies.
2. **UNION and EXISTS classification** — these are treated as outside the
   §5.1 v0.1 read subset (the Level R list names neither). Recorded in the
   TCK harness; flagged for your eye since it shapes the published number.
3. **CLI Strict-vs-Lenient binding** — the `query` command binds Strict
   when the schema declares structure, else Lenient (so schema-free Phase-1
   data stays queryable). A latent edge exists for when a CLI
   `put-schema` lands (Strict would reject a present-but-undeclared label);
   recorded on acetone-yzc.6.
4. **Two executor deviations from the spec's operator model** (recorded on
   the beads): scan/expand operators are the pattern matcher's internals
   rather than reified operator structs, and `IndexSeek` is deferred to
   Phase 5 (no physical index maps yet — index hints are consumed as
   scan+filter). Both are behaviour-neutral; flagged for retrospective.
