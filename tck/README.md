# tck — openCypher TCK conformance harness

- `features/` — the vendored TCK corpus (pinned; provenance and bump
  procedure in `features/README.md`).
- `src/` — the `acetone-tck` crate: Gherkin loading with outline
  expansion, the exact step-vocabulary matcher (unknown steps are hard
  errors), honest scenario classification, and the conformance report.
- `tests/` — the harness gate: the whole corpus must load and classify,
  and the load-time normalisation is pinned to its known sites.

Run it:

```bash
cargo run --release -p acetone-tck --bin tck_runner -- --report tck-report.json
```

CI runs exactly that per commit and uploads `tck-report.{json,txt}` as
the `tck-conformance-report` artefact. The job gates on the harness
completing, never on the pass rate — Gate C sets the bar at the Phase 2
boundary. Classification semantics (what may count as Passed or Failed
under the current parse-only backend) are documented in `src/lib.rs` and
`src/classify.rs`.
