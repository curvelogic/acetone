# Gate B spike: parser adoption (bead acetone-yzc.1)

Evidence for ADR-0013. Two candidate parsers are run against a
representative query set (`src/queries.rs`, 34 queries: spec §5.1 Level R
and W subsets, the roadmap's asset-registry queries, spec §5.2 acetone
extensions and procedures, plus deliberately invalid inputs).

- **Spike A** (`cargo run --bin decypher-eval`): the
  [decypher](https://github.com/sunsided/decypher) crate, 0.2.0-alpha.6 —
  the crate the spec names as its example of a spanned-AST parser.
- **Spike B** (`cargo run --bin handrolled-eval`): a hand-rolled
  recursive-descent parser slice (`src/handrolled/`, ~1,500 lines with AST
  and lexer) written for this spike — the "own the grammar" option,
  including the awkward corners (pattern predicates vs parenthesised
  expressions, list comprehensions vs list literals) and the `AT <ref>`
  extension.

`cargo test` keeps both halves honest: the hand-rolled parser must accept
every valid query and reject every invalid one, and decypher's gap list is
pinned so we notice if the crate improves.

## Results at spike time (2026-07-04)

| | decypher 0.2.0-alpha.6 | hand-rolled slice |
|---|---|---|
| Valid queries parsed | 26/31 | 31/31 |
| Level R gaps | list comprehensions, pattern predicates | none in set |
| `AT <ref>` extension | rejected (fork required) | one token of lookahead |
| Invalid inputs | rejected; one "internal error" message | rejected, positioned errors |
| Dependencies | rowan, thiserror, ryu, unicode-ident (+ miette, serde) | none |

This crate is excluded from the main workspace (like
`spikes/prolly-git-spike`) and never gates the build. It is evidence, not
product code — the real parser lands in `acetone-cypher` under bead
acetone-yzc.2.
