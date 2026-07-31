# openCypher conformance statement

acetone implements a subset of [openCypher][opencypher] and publishes its
conformance against the [openCypher TCK][tck] on every commit (spec §5.1). This
statement records the pass rate, how it is measured, what the measurement does
**not** verify, and the known gaps.

> Measured against TCK commit `677cbafabb8c3c5eed458fd3b1ec0daec8d67d23`. The
> live number is produced by the CI job "openCypher TCK conformance report"
> (`cargo run --release -p acetone-tck --bin tck_runner`); this document is
> refreshed at each release. Last refreshed 2026-07-31 (Phase 9 boundary,
> post-reopening) by running the runner on `main`; every figure was
> re-verified identical after the criterion-3 remediation PRs (#224–#230),
> which touched no TCK-visible behaviour.

## Pass rate

**2185 / 3897 scenarios pass (56.07%). 0 / 3897 fail.**

Percentages are shown to 2dp and never rounded up to clear a bar: 2185/3897 is
56.0688%, published as 56.07%. The discipline exists because a rounded number
once read as "55.0%" while the exact fraction (2142/3897 = 54.9654%) sat below
the 55% bar (PR #200) — so the fraction, not the rendering, is the claim.

| Area | Scenarios | Passing | Rate |
|------|-----------|---------|------|
| expressions | 2616 | 1241 | 47.44% |
| clauses | 1251 | 933 | 74.58% |
| useCases | 30 | 11 | 36.67% |

The full outcome breakdown:

| Outcome | Count | Meaning |
|---------|-------|---------|
| **passed** | 2185 | executed and matched the TCK-expected result — or correctly refused a query the TCK required to be refused (274 of the 2185; see disclosures) |
| unsupported (deferred syntax) | 1138 | a feature acetone declines by design — mostly at *bind* time, not parse: 1114 of the 1138 parse fine |
| unsupported (compile classification) | 307 | a compile-time error the TCK expects that acetone classifies differently |
| unsupported (executor) | 267 | parsed and planned, but the executor lacks the operator |
| **failed** | 0 | acetone rejects a query the TCK requires to be valid, or returns a wrong result |

Parser coverage over the queries under test: **3846 / 3897 parse**. Every one
of the 51 residual rejections is accounted for below — the exit criterion for
this phase required each to be individually listed and justified, so the runner
emits them by name (`parse.rejections` in the JSON report) rather than as a
bare count. By verdict those 51 split 27 **passed** (the TCK demanded a syntax
error and got one) and 24 **unsupported**; none is a failure.

"Unsupported" outcomes are **honest declines**, not wrong answers: acetone
reports a typed "not supported" rather than mis-executing. **Zero failures**
means no scenario in the corpus makes acetone return a wrong result. It does
not mean nothing is wrong: two scenarios (Set1 [3]/[4]) reject a query the TCK
requires to be valid, and the harness's write-query escape files them as
deferrals rather than failures — see the disclosures below.

## Every residual parse rejection, justified

**27 of the 51 are correct rejections**: the scenario's own name begins "Fail
on…"/"Fail when…"/"Failing on…" — the TCK asks for a syntax error there, and
acetone produces one. They are rejections *because the query is invalid*.

| Family | Count | What acetone rejects |
|--------|-------|---------------------|
| `expressions/literals` Literals8 | 8 | malformed map literals (numeric/symbol/dotted keys, missing keys, unmatched braces) |
| `expressions/literals` Literals3 | 5 | malformed hexadecimal integers (incomplete, invalid digits, out of range) |
| `expressions/literals` Literals2 | 4 | malformed decimal integers (out of range, alphabetic or symbol characters) |
| `expressions/literals` Literals7 | 3 | malformed list literals (stray comma, unmatched brackets, missing commas) |
| `expressions/literals` Literals4 | 2 | out-of-range octal integers |
| `expressions/literals` Literals6 | 1 | a truncated unicode escape |
| `clauses/match` Match4 | 2 | malformed variable-length bounds (missing asterisk, negative bound) |
| `clauses/call` Call5 | 1 | `YIELD *` in a mid-query `CALL` (legal only in a standalone `CALL`) |
| `expressions/mathematical` Mathematical3 | 1 | a Unicode en-dash used as a subtraction operator |

**22 are whole features acetone does not parse by design**, each declining
cleanly:

| Feature | Count | Scenarios |
|---------|-------|-----------|
| `UNION` / `UNION ALL` | 12 | Union1 [1]–[5], Union2 [1]–[5], Union3 [1]–[2] |
| Existential subqueries (`EXISTS { … }`) | 10 | ExistentialSubquery1 [1]–[4], 2 [1]–[3], 3 [1]–[3] |

**2 are genuine parser gaps** — the only entries on this list that represent
work acetone owes, and they are *mis-bucketed in acetone's favour*: both are
write queries, so the harness's write-query escape (described below) files them
as deliberate deferrals rather than failures.

| Scenario | Query shape | Bead |
|----------|-------------|------|
| `clauses/set` Set1 [3] | `SET (n).name = 'neo4j'` — a parenthesised expression as a `SET` target | `acetone-2ck.12` |
| `clauses/set` Set1 [4] | the same shape on a relationship | `acetone-2ck.12` |

So of 3897 queries under test, **two** are rejected by a parser limitation
acetone intends to fix; the rest of the residue is either correct rejection of
invalid syntax or deliberate deferral.

## What the measurement does not verify

A pass rate is only as honest as its harness. These are the ways this one is
weaker than the number alone suggests. **Three of them can, in principle, turn
a defect into a pass**; they are listed first, because those are the ones that
matter.

### Can convert a defect into a pass

- **274 of the 2185 passes (12.5%) never execute the query.** They are credited
  on a front-end rejection where the TCK expected a compile-time error. 247 of
  those are *binder* rejections matched on both the error class and the TCK's
  detail string — a genuinely strong check. The other **27 are parse rejections
  where the rejection reason is not verified at all**: any parse error
  whatsoever earns the pass, because the parser cannot know which error the TCK
  meant (`tck/src/classify.rs`, the `ParseRejected … SyntaxError` arm). A query
  rejected for the wrong reason still passes.
- **Entity comparison is structural, not identity-based.** Expected nodes and
  relationships are compared by label-set/type and properties; the TCK's entity
  identity is never consulted. If acetone returns the *wrong* entity and it is
  structurally identical to the right one, the comparator credits a pass.
- **A write-query escape hatch reclassifies would-be failures as deferrals.**
  Any front-end rejection of a query containing a `CREATE`/`MERGE`/`SET`/
  `REMOVE`/`DELETE` token is bucketed as deferred syntax rather than failed,
  whether or not the construct is genuinely deferred — the token test is on raw
  text, not on a bound plan. The live example is `SET (n).name = 'neo4j'`
  (Set1 [3]/[4]) above: plain openCypher, a real parser gap, counted as a
  deliberate deferral. This directly props up the "zero failures" headline.
  Tracked as `acetone-s1j` (gate the test on the bound plan, not raw tokens).

### Conservative — can leave a defect uncounted, never inflate the count

- **Undirected path-step notation in expected values is unsupported** — such
  scenarios classify as unsupported rather than being verified.
- **`lists_unordered` expectations are unsupported**, so scenarios whose
  expected list order is explicitly free are not verified.
- **Control expectations of `Error` and `None` are not verified** against the
  specific error the TCK names.
- **Setup failures bucket as unsupported.** If a scenario's `Given`/setup steps
  cannot be built, the scenario is unsupported rather than failed — which can
  hide an engine defect behind a setup limitation.

## How the number is kept honest

The Phase 9 bar was: no TCK scenario fails at parse, every residual rejection
individually listed and justified (above), and a published pass rate ≥ 55%,
gaming-resistant. Gaming resistance rests on three properties a reader can
check in this repository:

- **CI gates on completing, never on the rate.** An unreadable corpus or an
  unknown step vocabulary fails the build; the pass rate itself is reported,
  not enforced. There is no incentive to tune the number rather than the
  engine.
- **Classification is by outcome, with no scenario-name-keyed logic anywhere.**
  No allow-list, no expected-failure file, no scenario skipped by name; the
  keyword lists in `tck/src/classify.rs` route only *away* from `Passed`. This
  is the strongest structural claim here and it is verifiable by reading that
  one file. It is not a blanket reassurance: the write-query token test
  disclosed above routes away from `Failed`, which is the flattering
  direction.
- **Every pass-rate-moving change is reviewed adversarially** by a fresh
  reviewer with no implementation context, briefed to hunt over-crediting. The
  record of those reviews lives in the bead trail and PR descriptions, not in
  committed tests — a reader can read the reasoning but cannot re-run it, which
  is a weaker guarantee than the two properties above.

The 12.5% of passes credited without executing (see the disclosures above) is
the standing caveat on all of this: the rate measures conformance *including*
correctly-refused invalid queries, which is what the TCK asks for, but it is
not 2185 successfully executed queries.

## What is solid

The passing core is the workbench's daily surface: `MATCH`/`WHERE`/`RETURN`
over node and relationship patterns, property access and comparison, the
openCypher null three-valued logic (TCK-verified), `ORDER BY`/`SKIP`/`LIMIT`
(with `WITH`'s sub-clause order matching openCypher exactly), list and map
literals and indexing, arithmetic and string/list functions, pattern
comprehensions, label predicates in expression position, `CALL … YIELD`
aliasing, chained comparisons, `CREATE`/`SET`/`MERGE`/`DELETE` write semantics,
and `WITH` pipelines. Null semantics in particular follow openCypher exactly
rather than approximately.

## Known gaps

The gaps are whole feature families rather than individual defects — the
per-scenario failure backlog is empty. The families are not what the bucket
names suggest, so they are given here by what the queries actually contain.

| Gap family | Bucket | Scale | Bead |
|-----------|--------|-------|------|
| **Temporal types and functions** (`datetime`, `localdatetime`, `duration`, `date`, `time`, `localtime`) | deferred syntax | ~1025 scenarios — 90% of that bucket | `acetone-hi8` |
| Other unimplemented functions (`rand`, `percentileDisc`/`percentileCont`) | deferred syntax | ~78 scenarios | — |
| `UNION` / `UNION ALL` and existential subqueries | deferred syntax | 22 scenarios | — |
| Assorted remainder of that bucket (the two Set1 write-escape scenarios, six that parse but carry a deferred token, five other bind rejections) | deferred syntax | 13 scenarios | — |
| Compile-time error **classification** differences: acetone raises a typed error, but a different class or detail than the TCK names | compile classification | 307 scenarios | — |
| Executor operators not yet implemented | executor | 267 scenarios (expressions 130, clauses 118, useCases 19) | — |

Two corrections to the impression the bucket names give. First, "deferred
syntax" is mostly a *bind-time* decline on unimplemented functions, not a
parsing gap: 1114 of its 1138 scenarios parse without complaint. Second,
temporal support — not subquery or CSV syntax — is by a wide margin acetone's
largest single conformance gap. `CALL {}` and `FOREACH` — headline items in
spec §5.1's deferral list — plus `LOAD CSV`, which is simply outside the v0.1
subset, appear in **zero** files in this corpus, so they cost nothing here.

Of these families, §5.1 explicitly defers only full temporal *arithmetic*; the
temporal type constructors (`date()`, `datetime()`, …) that account for most of
those 1025 scenarios are unimplemented rather than deferred by design.
The compile-classification and executor buckets are not design decisions: the
first is a genuine conformance divergence (right to reject, wrong error class),
the second is simply unimplemented.

## How to read a regression

A drop in `passed` with a rise in `failed` is a **regression**. A rise in
`unsupported_*` with no `failed` change is a **scope decision**, not a bug.
Because the corpus currently has zero failures, any non-zero `failed` count in a
future report is a regression by definition and should block the change that
caused it.

[opencypher]: https://opencypher.org/
[tck]: https://github.com/opencypher/openCypher/tree/master/tck
