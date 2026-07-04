# ADR-0013: Gate B — build the Cypher parser, don't adopt decypher

*Status: accepted · Date: 2026-07-04 · Bead: acetone-yzc.1 · Evidence:
spikes/cypher-parser-spike*

## Context

Roadmap Gate B (start of Phase 2): adopt the decypher crate or vendor the
openCypher grammar for `acetone-cypher`'s front end. Spec §5.3 requires a
spanned AST; §5.1 fixes the Level R subset the parser must cover; §5.2 adds
non-standard syntax (`AT <ref>` suffixing a MATCH clause group). The bead's
acceptance criteria required a spike of each option against a
representative query set — 34 queries covering Level R, Level W, the
roadmap's registry queries, the acetone extensions, and invalid inputs.

"Vendor the grammar" has one practical form in Rust: a hand-written
recursive-descent parser tested against the openCypher EBNF and TCK.
antlr-rust is effectively unmaintained (no release since ANTLR 4.8-era,
2022) and generating from the official grammar is not a supported path;
ocg's pest parser is embedded in a 26-dependency engine crate and not
adoptable separately; open-cypher (pest) is early-stage and dormant since
February 2025.

## Decision

**Build the parser in `acetone-cypher`**: hand-written spanned lexer and
recursive-descent/Pratt parser, grown under the TCK harness (acetone-yzc.3)
with the openCypher EBNF as the reference grammar. Do not adopt decypher.
Keep the front end behind a narrow parse-to-AST boundary so the choice is
revisitable (the spec's GQL-drift concern) without touching binder or
planner.

The spike is the evidence, not the product; acetone-yzc.2 starts from its
shape (spanned AST, no-panic error type, the disambiguation strategies)
rather than its code.

## Consequences

- **Why not decypher**: at 0.2.0-alpha.6 it fails two Level R requirements
  outright (pattern predicates, list comprehensions: 26/31 valid queries
  parsed), cannot express `AT <ref>` — a fork of its hand-written rowan
  grammar would be needed, at which point we own a parser anyway, just
  someone else's — and it is a single-maintainer alpha with an explicitly
  unstable AST. Its diagnostics are attractive but include an
  "internal error" message on plain bad input.
- **What the spike showed about building**: the hand-rolled slice parsed
  31/31 valid queries including both extension queries (`AT` costs one
  token of lookahead) and rejected all invalid inputs with positioned
  errors, in ~1,500 lines written inside the gate's timebox. The awkward
  corners (pattern predicate vs parenthesised expression, comprehension vs
  list literal) have known, bounded resolutions.
- **The honest cost**: full openCypher is much larger than the spike —
  temporal literals, `EXISTS`, quantified expressions, escape/unicode
  rules, reserved-word edge cases. That cost lands exactly where the TCK
  harness applies pressure, and the published pass rate keeps it honest;
  with the openCypher grammar vendored as reference there is no dependency
  that can drift, die, or cap our conformance.
- **Foreclosed/revisit**: nothing structural is foreclosed — the parse
  boundary keeps adoption open. The spike pins decypher's gap list in a
  test; if decypher reaches the Level R bar and stabilises post-1.0, or if
  TCK-driven grammar growth stalls Phase 2, this decision is revisited.
  Mirrors the Gate A prollytree precedent: adopt-vs-build settled by
  running-code evidence, and build won on the same grounds (the dependency
  fails the load-bearing requirement; owning the component is cheaper than
  owning a fork).
