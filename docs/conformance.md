# openCypher conformance statement

acetone implements a subset of [openCypher][opencypher] and publishes its
conformance against the [openCypher TCK][tck] on every commit (spec §5.1). This
statement records the pass rate, how it is measured, and the known gaps.

> Measured against TCK commit `677cbaf`. The live number is produced by the CI
> job "openCypher TCK conformance report" (`cargo run --release -p acetone-tck
> --bin tck_runner`); this document is refreshed at each release. Last
> refreshed 2026-07-23 (for 0.3.0) by running the runner on `main`.

## Pass rate

**1598 / 3897 scenarios pass (41.0%).**

| Area | Scenarios | Passing | Rate |
|------|-----------|---------|------|
| expressions | 2616 | 1097 | 41.9% |
| clauses | 1251 | 501 | 40.0% |
| useCases | 30 | 0 | 0.0% |

The remaining scenarios break down as:

| Outcome | Count | Meaning |
|---------|-------|---------|
| **passed** | 1598 | executed and produced the TCK-expected result |
| unsupported (deferred syntax) | 1137 | a language feature acetone deliberately does not parse yet |
| unsupported (executor) | 802 | parsed and planned, but the executor lacks the operator |
| unsupported (compile classification) | 306 | a compile-time error the TCK expects that acetone classifies differently |
| **failed** | 54 | acetone rejects a query the TCK requires to be valid, or returns a wrong result |

"Unsupported" outcomes are **honest declines**, not wrong answers: acetone
reports a typed "not supported" rather than mis-executing. The 54 **failures**
are the real conformance bugs and the improvement backlog.

## What is solid

The passing core is the workbench's daily surface: `MATCH`/`WHERE`/`RETURN`
over node and relationship patterns, property access and comparison, the
openCypher null three-valued logic (TCK-verified), `ORDER BY`/`SKIP`/`LIMIT`,
list and map literals and indexing, arithmetic and string/list functions,
`CREATE`/`SET`/`MERGE`/`DELETE` write semantics, and `WITH` pipelines. Null
semantics in particular follow openCypher exactly rather than approximately.

## Known gaps

Each is tracked; the pass rate climbs as these land.

| Gap | TCK impact | Bead |
|-----|-----------|------|
| Label predicate in expression position (`WHERE n:Label`, incl. self-loops) | 17 failures | `acetone-6gy` |
| Pattern comprehension `[ (a)-[r]->(b) WHERE p \| expr ]` | 16 failures | `acetone-cxh` |
| `CALL … YIELD a AS b` aliasing and `YIELD *` | 12 failures | `acetone-i8z` |
| Bidirectional relationship pattern (`(a)<-[r]->(b)`) | 4 failures | triaged in the report JSON |
| `MERGE`-relationship `ON CREATE` columns / `SET = <entity>` | 4 failures | `acetone-q9m` |
| Default result-row limit trips one huge summation scenario | 1 failure | triaged in the report JSON |

Larger **deferred-syntax** families (the 1137 above) — e.g. `CALL {}`
subqueries, `FOREACH`, `LOAD CSV`, quantified path patterns, `UNION` variants —
are out of scope for 0.1 by design (spec §5.1, "Explicitly deferred"). They decline
cleanly rather than fail.

## How to read a regression

The harness gates on **completing** (an unreadable corpus or unknown step
vocabulary fails CI), never on the pass rate itself, so a conformance change is
visible in the report without breaking the build. A drop in `passed` with a rise
in `failed` is a regression; a rise in `unsupported_*` with no `failed` change is
a scope decision, not a bug.

[opencypher]: https://opencypher.org/
[tck]: https://github.com/opencypher/openCypher/tree/master/tck
