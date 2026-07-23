# openCypher conformance

acetone's query language is a subset of [openCypher](https://opencypher.org/),
and its conformance is measured — on every commit — against the official
[openCypher TCK](https://github.com/opencypher/openCypher/tree/master/tck)
(Technology Compatibility Kit), a corpus of 3,897 executable scenarios. The
authoritative, per-release statement is the repository's
[conformance statement](https://github.com/curvelogic/acetone/blob/main/docs/conformance.md);
this appendix explains how to read it.

## The current pass rate

**1,598 of 3,897 scenarios pass (41.0%)**, measured 2026-07-23 against TCK
commit `677cbaf` by running the harness on `main`
(`cargo run --release -p acetone-tck --bin tck_runner`). By area: expressions
1,097 of 2,616, clauses 501 of 1,251, useCases 0 of 30.

## What the classification means

A headline percentage hides the distinction that matters for an operator:
*what happens when acetone meets a query it does not support?* Every TCK
scenario lands in exactly one bucket:

| Outcome | Count | Meaning |
|---------|-------|---------|
| **passed** | 1,598 | executed and produced the TCK-expected result |
| unsupported (deferred syntax) | 1,137 | a language feature acetone deliberately does not parse yet |
| unsupported (executor) | 802 | parsed and planned, but the executor lacks the operator |
| unsupported (compile classification) | 306 | a compile-time error the TCK expects that acetone classifies differently |
| **failed** | 54 | acetone rejects a query the TCK requires to be valid, or returns a wrong result |

The classification is kept **honest** by construction: a scenario is only ever
*Passed* when it executed and matched the TCK's expected rows and side
effects, and *Unsupported* only when acetone returned a typed "not supported"
rather than an answer. An unsupported query **declines cleanly — it is never
mis-executed into a wrong result** and never silently counted as anything
else. The 54 *failures* are the real conformance bugs, each triaged in the
conformance statement's known-gaps table with a tracking bead.

For daily workbench use, the passing core is the surface that matters:
`MATCH`/`WHERE`/`RETURN` over node and relationship patterns, openCypher's
three-valued null logic (TCK-verified), `ORDER BY`/`SKIP`/`LIMIT`, list and
map literals, arithmetic and string/list functions,
`CREATE`/`SET`/`MERGE`/`DELETE` write semantics, and `WITH` pipelines. The
large deferred-syntax families — `CALL {}` subqueries, `FOREACH`, `LOAD CSV`,
quantified path patterns, `UNION` variants — are out of scope by design (spec
§5.1) and decline cleanly.

## The harness

The TCK runner lives in the repository (`tck/`), vendoring the scenario
corpus at a pinned upstream commit. Each scenario's setup graph and query are
executed for real, and both the result rows *and* the side effects (as a
graph-state delta) are verified against the TCK's expectations.

CI runs the full corpus on every commit (the "openCypher TCK conformance
report" job) and publishes the report. The job gates on **completing** — an
unreadable corpus or unknown step vocabulary fails CI — never on the pass
rate itself, so a conformance change is visible in the report without
breaking the build. Reading a change: a drop in *passed* with a rise in
*failed* is a regression; a rise in *unsupported* with no *failed* change is
a scope decision, not a bug.
