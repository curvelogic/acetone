# ADR-0046: Freeze the `acetone-core` public API at 0.2

- Status: accepted (Greg's 0.2 boundary decision, 2026-07-18)
- Date: 2026-07-19
- Deciders: Greg (directed the freeze and the automated-enforcement mechanism at the Phase 7 / 0.2 boundary); agent under the Phase 7 mandate
- Related: spec §7 (the library API "stabilises at 0.2, gated on the query-engine resource governor"); ADR-0026 (`acetone-core` façade + packaging); ADR-0024 (the Gate D *format* freeze, whose golden pins this mirrors for the API); ADR-0036/0038/0039/0043 (the frozen query surface — governor, value carrier, `Session`, advisories); bead `acetone-2p9`

## Context

Spec §7 says the `acetone-core` library API "stabilises at 0.2, gated on the
query-engine resource governor". That gate is met: the governor shipped
(ADR-0036), and the query surface it configures — `Session`, `QueryLimits`,
`QueryResult`, the runtime value carrier, relationship identity — was
deliberately shaped this phase (ADR-0036/0037/0038/0039/0043) so it would be
sound to freeze. Greg directed the freeze at the boundary.

Two things were unresolved before this ADR:

- **Scope.** `acetone-core` re-exports whole crates (`pub use acetone_cypher as
  cypher`, …) *and* a curated set of headline types flat at the crate root.
  Freezing the entire transitive surface of four crates would pin vast internals
  and make every internal change a breaking one.
- **Enforcement.** The format freeze is enforced by byte-exact golden pins
  (ADR-0024). An API freeze needs the analogue — a mechanism that fails loudly
  when the public surface drifts — or "frozen" is only a promise in prose.

## Decision

**1. Freeze the curated headline surface; deep access is unstable.** The types
and functions re-exported flat at the `acetone-core` crate root are the
**semver-guaranteed** 0.2 API. The whole-crate module re-exports (`cypher`,
`graph`, `model`, `store`) remain available as **deep access** and are **not**
covered by the guarantee — items reachable only through them may change in any
0.2.x release. The curated surface was completed as part of this work:
`QueryLimits`, `QueryResult` and `ResourceLimit` are now re-exported at the root
(ADR-0036/0043 had declared them frozen, but they were reachable only through
the deep-access modules). Likewise the **runtime value type** — the element type
of `QueryResult` rows and of the `run_with` parameter map (ADR-0038) — is
re-exported as **`QueryValue`**: it is transitively part of the frozen query API
(you cannot read a row or bind a parameter without it), so leaving it reachable
only through the unstable `cypher` module would make the contract incoherent on
its main path. It is distinct from the stored-domain `Value` (keys/records).

**2. Semver policy.** The frozen surface follows semver **additive-only within
the 0.2.x series**; a breaking change to it requires **0.3.0**. `STABILITY.md`
records the surface and the policy; the crate-root docs mark stable vs deep.

**3. Version bump `0.1.1 → 0.2.0`** (Greg approved). The frozen surface *is*
0.2.0. This does not cut a release — tagging and the GHA/Homebrew artefacts stay
a later, Greg-gated step, exactly as the 0.1 tag was separate from the format
freeze.

**4. Enforce with committed `cargo-public-api` snapshots.** Two snapshot files
are committed and a dedicated CI job regenerates them and fails on any diff —
the API golden. The scope is split because `acetone-core` is a **pure façade**:
`cargo-public-api` snapshots one crate at a time and treats a cross-crate
`pub use` as opaque, so `acetone-core`'s own snapshot is the *list* of exposed
symbols, not the signatures behind them (those are defined in the sibling
crates). So (Greg's scope choice at the boundary):

- **`crates/acetone-core/public-api.txt`** — the curated re-export **list**. It
  catches the semver-significant events on the frozen surface: a symbol added,
  removed, or renamed. (Adding is a minor bump; removing/renaming is breaking.)
- **`crates/acetone-cypher/public-api.txt`** — the **full-signature** public API
  of `acetone-cypher`, which hosts the *newest* and most-likely-to-churn frozen
  query surface (`Session`, `QueryLimits`, `QueryResult`, the runtime value
  carrier, `ResourceLimit`). This catches signature-level drift — a changed
  field, a new method, a new variant — automatically.
- The older, stabler crates (`graph`, `model`, `store`) are guarded by the
  façade list plus the fresh-review gate rather than their own full-signature
  snapshots — a deliberate friction/coverage trade (a full-signature snapshot of
  every workspace crate would trip on ordinary internal-`pub` refactors).

**No new runtime or dev dependencies.** `cargo-public-api` is a CI-only binary,
not a crate dependency, so the shipped artefact and the normal build are
untouched. (Rejected: adding `public-api`/`rustdoc-json` as dev-deps — they
would compile into every `cargo test` and enlarge the dependency surface for a
check only CI needs.) The binary is installed in CI with `cargo install
--locked --version 0.52.0` rather than a SHA-pinned action, a slightly weaker
supply-chain posture than the rest of the workflow; `--locked` (its own
lockfile) and the exact version pin mitigate it, and it is worth revisiting a
vendored/cached binary if the freeze proves load-bearing.

## Consequences

- A library consumer gets a stable, documented API and a machine-checked
  guarantee that it will not drift silently within 0.2.x.
- **Tooling pin, with a real maintenance cost (recorded honestly).**
  `cargo-public-api` reads rustdoc's *unstable* JSON, whose `format_version`
  moves with nightly. The CI job therefore pins **both** a nightly toolchain and
  a `cargo-public-api` version known to parse that nightly's format (at
  introduction: `nightly-2026-07-18`, rustdoc JSON `format_version` 60,
  `cargo-public-api` 0.52.0). Bumping the nightly means bumping the tool and
  re-blessing the snapshot together; the pin is stated in the CI job and in
  `STABILITY.md`. This fragility is the price of an automated API golden and was
  accepted over the lighter policy-doc option.
- Regenerating (blessing) the snapshot after an intended change is a documented
  one-liner; the CI job's failure message points to it.
- No `format_version` bump, no on-disk change: this is a source-level API and
  packaging decision only. Load-Bearing Invariants 1–5 are untouched.
- The CLI's `--json` output stability is a **separate** surface (spec §7: the
  CLI is its own product) and is explicitly not covered here; reconciling its
  "may change before 0.2" wording at the actual 0.2 release is a filed
  follow-up.
- Rejected: freezing the whole transitive surface (pins internals, every change
  breaking); dev-dependency enforcement (dependency-surface cost); a
  policy-doc-only freeze (no mechanical drift detection — the option Greg
  declined).
