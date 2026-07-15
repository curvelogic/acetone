# ADR-0036: Query-engine resource governor

- Status: accepted
- Date: 2026-07-15
- Deciders: Greg (enforcement-mechanism gate, ruled at the 0.2 kick-off); agent under the Phase 7 mandate
- Related: Phase 2 milestone security review (MAJOR-1, MAJOR-2); spec §5 (query language); beads `acetone-iq6` (this work, absorbs `acetone-18z`), `acetone-vf6` (library query entry point — freezes this config), `acetone-ijq` (route CLI through the core facade); Load-Bearing Invariants (this ADR adds a determinism obligation)

## Context

The read-path query engine has **no resource governor**. A single untrusted
query can drive unbounded CPU or memory, confirmed exploitable in the Phase 2
security review:

- **(a)** unbounded var-length `MATCH (a)-[*]->(b)` enumerates every simple
  path — a complete 9-node/72-edge graph did not finish in 20 s;
- **(b)** eager list materialisation — `RETURN range(0, 10000000000)` was
  OOM-killed at ~80 GB.

For the CLI (the operator runs their own queries) this is self-inflicted, but
acetone's stated **Rust-library embedding** may feed end-user queries, which
makes an unbounded engine the top hardening priority for the 0.2 gate. The
governor's configuration surface is part of what `acetone-core` **freezes as
public API** at 0.2 (`acetone-vf6`), so its shape is a decision, not an
implementation detail.

The executor is a **materialised clause pipeline**, not a streaming iterator
model: `run_versioned` (`crates/acetone-cypher/src/exec/run.rs`) is the single
funnel every read and write query passes through, and row sets are fully
materialised `Vec<Row>` between clauses. There is one pre-existing cap —
`MAX_RANGE_ELEMENTS = 10_000_000` in `functions.rs`, whose own comment flags it
for this bead. `LIMIT` truncates *after* full materialisation, so it bounds
output, not work.

## Decision

Add a configurable governor whose **canonical cap is a deterministic work
budget**, with wall-clock time as an **optional, off-by-default backstop**
(Greg's ruling, 2026-07-15).

The alternative — wall-clock time as the primary cap — was rejected: it makes
whether a query succeeds or errors depend on machine speed, so it is not
reproducible in property tests and cuts against acetone's determinism ethos
(history independence, deterministic encodings, merge as a pure function). The
frozen public contract is therefore a **work budget**: the same query over the
same graph yields the same success/error on every machine.

### Public configuration surface (frozen at 0.2)

```rust
pub struct QueryLimits {
    pub max_work_units:      u64,             // canonical odometer — total charged work
    pub max_result_rows:     u64,             // any Vec<Row> length, at every growth site
    pub max_expansion_steps: u64,             // cumulative var-length hops popped in DFS
    pub max_collection_len:  u64,             // any single list/collection (generalises range())
    pub wall_clock:          Option<Duration>, // backstop; Default = None
}
```

`Default`: `100_000_000` work units, `1_000_000` result rows, `1_000_000`
expansion steps, `10_000_000` collection length (retains today's `range()`
cap), `wall_clock: None`. Realistic registry/lab-graph queries run orders of
magnitude under these; the two known exploits trip in well under a second.
Defaults are **validated, not asserted** — the property/fuzz regime (0.2 exit
criterion 2) is what proves no query escapes them. `wall_clock = None` by
default keeps the default execution path both deterministic and zero-cost (no
clock reads).

### Mechanism

- A `Governor` is owned by `run_versioned` (the single funnel) and holds
  `Cell<u64>` counters — the executor is single-threaded, so `Cell` beats
  atomics. It is threaded into `EvalCtx` by shared reference (`&'a Governor`),
  mirroring the existing interior-mutable `aggregates: Cell<_>` precedent.
  It **must** be outer-owned: `EvalCtx` is rebuilt per clause, so a value field
  would reset the counters every clause.
- Methods `charge_work(n)`, `charge_rows(len)`, `charge_expansion(1)`,
  `charge_collection(len)`, each returning `Result<(), ExecError>`. Wall-clock,
  when `Some`, is polled off an `Instant` every N work units so the common
  `None` path never reads the clock.
- A new error variant `ExecError::ResourceExceeded { limit: ResourceLimit,
  span }`, where `ResourceLimit ∈ {WorkUnits, ResultRows, ExpansionSteps,
  CollectionLen, WallClock}` carries the cap that was hit. It propagates via `?`
  and renders through the existing plumbing — a clean bounded-resource error,
  never a hang or OOM.

### Enforcement seams

`row` at every row-set growth site (`match_clause` — including the
cross-pattern cartesian intermediate set — `UNWIND`, `project`, `collect`,
CALL, and the create/merge/set/delete write helpers); `hop` per edge in both
the fixed-length walk and the `expand_var_length` explicit-stack DFS;
`collection`/`collection_push` for every value that materialises unboundedly —
`range()` (generalising the former `MAX_RANGE_ELEMENTS`), list literals and
comprehensions, `collect()`, and crucially the `+` operator's list/string
concatenation (the one operator that produces a value unboundedly larger than
its inputs, e.g. a doubling `reduce`); and a per-iteration charge in `reduce`
and the quantifiers so their per-element work is accounted, not just bounded by
the source list. The rule throughout is **charge before allocate**: a single
oversized step is rejected up front, never after the memory is spent.

### Invariants

The governor is execution-time only: it touches no key/value encoding, no
prolly-tree root, and no merge, so **Load-Bearing Invariants 1–5 are
untouched**. It *adds* a determinism obligation — the work-unit accounting must
itself be reproducible (same query + graph ⇒ identical charged work and
identical success/error) — enforced by a determinism property test that lands
with the code. Keeping `wall_clock` off by default is what preserves
determinism on the default path.

## Consequences

- The three named pathologies return a bounded `ResourceExceeded` error instead
  of hanging or OOMing; caps are documented and configurable.
- `acetone-core`'s frozen query entry point (`acetone-vf6`) can take a
  `QueryLimits` and expose a deterministic bound as part of the 0.2 contract.
- A work budget is more work to account for than a single wall-clock timeout
  (every growth seam must charge), but it buys reproducibility — the property
  regime the exit criteria demand is only possible against a deterministic cap.
- Rejected: wall-clock as the primary cap (non-reproducible, above); a
  thread-local governor (hidden global, hostile to library embedding and to the
  determinism test); a governor value-field on `EvalCtx` (counters reset per
  clause).
