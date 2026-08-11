# ADR-0076: Cross-language surface strategy — protocol-first

*Status: accepted — ruled by Greg at the Phase 12 opening (2026-08-11),
adopting the proposal recorded at the 0.6 boundary grooming · Date:
2026-08-11 · Bead: acetone-zavr.1*

## Context

Greg asked at the 0.6 boundary whether the daemon is the right approach
to cross-language use, versus a C ABI or per-language native bindings.
The assessment: acetone sits naturally in the protocol-first-with-thin-
clients camp (the Dolt/LSP/TigerBeetle pattern). The workload is
chunky, streamed operations (imports, queries, merges), making
per-round-trip overhead second-order — a qualitative argument today:
the latency measurement against process-per-command that ADR-0074
promised as phase evidence was never performed, and is now owed in this
phase's protocol-document work (`acetone-zavr.8`). The
writer lock is inherently cross-process, so in-process embedding
*distributes* the arbitration problem rather than removing it; process
isolation is a feature for a corruption-critical store; one versioned
wire contract beats an N×M binding/release matrix at this team size;
and a C ABI would leapfrog the deliberate crates.io withholding
(ADR-0047) with a harsher pre-1.0 contract. A hand-rolled C ABI has the
worst effort-to-safety ratio of the options (panic-across-FFI UB,
pointer/ownership conventions, segfaults inside host processes).

## Decision

1. **Protocol-first is the strategy.** The frame protocol (ADR-0074) is
   the canonical cross-language surface, promoted to a documented,
   versioned artefact, with the Python client as its reference
   implementation.
2. **A C ABI is declined.** If a C surface is ever demanded, it will be
   a tiny *generated* one (e.g. UniFFI over a minimal facade) — never a
   hand-rolled mirror of `acetone-core`.
3. **Native in-process bindings are demand-driven** and
   single-language-first, built directly over `acetone-core` (PyO3 for
   Python, napi-rs for Node), with no C intermediary — triggered only
   by a demonstrated need IPC cannot meet (e.g. serialisation-dominated
   bulk load, a wasm host).
4. **Companion feature:** `acetone serve --stdio` (LSP-style child
   process, `acetone-zavr.2`), so a host can embed the daemon without
   managing a socket.

## Consequences

- Phase 12's parity strand (ADR-0075) is what makes the protocol
  document worth versioning; the protocol doc and its version marker
  are part of that strand's deliverable.
- No binding crates, no cbindgen/UniFFI scaffolding, and no crates.io
  publication are implied by cross-language demand while this ruling
  stands; a future native binding starts from a tenant-demonstrated
  need, recorded as a decision bead.
- The Python client graduates from test harness to reference client —
  its conformance to the protocol document becomes a maintained
  property.
