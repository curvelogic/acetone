# ADR-0057: Encode the release process as a beads formula

*Status: accepted — experiment proposed by Greg at the 0.3.0 release (2026-07-23) · Date: 2026-07-23 · Bead: acetone-bxv*

## Context

The 0.3.0 release was executed ad hoc from `docs/RELEASING.md`, and the ordering
slipped: the Release workflow was dispatched *before* the version-bump/changelog
PR had landed on `main`, so the first build was of the wrong commit. The steps
of a release have hard dependencies (prep must land before build; only Greg may
publish; Homebrew follows publish), but nothing machine-checkable enforced them
— the ordering lived only in prose.

bd 1.0.5 ships formula/proto/molecule machinery for exactly this shape of work:
a TOML formula defines a dependency DAG of steps with `{{variable}}`
substitution; `bd cook` compiles it; `bd mol pour`/`bd mol wisp` instantiate it
as real beads whose children flow through `bd ready` in dependency order; a
`[steps.gate]` block parks a step until a human resolves it; `bd mol squash`
condenses a finished run into a permanent digest.

A hazard to design around: a formula that *duplicates* the commands in
`RELEASING.md` and `.github/workflows/release.yml` would drift from them the
first time either changes.

## Decision

**Encode the release flow as `.beads/formulas/release.formula.toml`** — a
six-step linear DAG with `{{version}}` as the only variable:

```
preflight → prep → land → build → publish (human gate) → post-publish
```

- **preflight** — main green, no stray drafts, scope agrees with the phase report.
- **prep** — version bump (workspace + six `acetone-*` pins), changelog cut,
  local check of the release-notes extractor.
- **land** — PR, adversarial review, squash-merge; *hard ordering*: build is
  blocked on this step, encoding the 0.3.0 lesson in the DAG itself.
- **build** — dispatch the Release workflow; verify 8 draft assets; checksum
  and smoke-test a binary.
- **publish** — carries a `human` gate: the molecule parks until Greg publishes
  the draft (which creates the tag) and the gate is resolved. No agent publishes.
- **post-publish** — verify and merge the *automated* Homebrew tap PR
  (`homebrew-bump.yml`, since PR #170 — not created by hand), verify tag and
  attestation, then `bd mol squash` to a digest.

Design rules:

- **Steps are pointers, not scripts.** Each step names the `RELEASING.md`
  section it executes plus acceptance criteria ("done when…"); commands stay in
  `RELEASING.md` and `release.yml`, so the formula cannot drift from them.
- **`RELEASING.md` remains the narrative authority**; the formula is the
  tracked execution path. `RELEASING.md` gains a short section pointing at it.
- **`phase = "vapor"`**: release runs are one-shot, so the recommended
  instantiation is `bd mol wisp` (ephemeral children), squashed to a permanent
  digest at the end — audit trail without fifty dead beads per release.
- **Governing-config class**: `.beads/formulas/` shapes agent workflow, so this
  file and future formulas take the full adversarial-review path plus an ADR,
  like `.beads/hooks/`.

Divergences from the bead's sketch, found against the real bd 1.0.5 machinery:

- The file must be named `release.formula.toml` — `bd formula list` does not
  discover a bare `release.toml`.
- `bd cook` does **not** enforce the `pattern` constraint on `--var` values in
  1.0.5 (`version=bogus` substitutes silently); the pattern is documentation
  plus forward-compatibility, and the preflight step is the real check.
- "Molecule parks via gate-resume" is real but manual-ish: the gate bead blocks
  the `publish` step; `bd gate resolve` (human gates never auto-close) frees
  it, and `bd mol ready`/`bd ready --gated` finds molecules to resume.

## Consequences

- **The 0.3.0 failure mode is structural now**: `build` is unready until `land`
  closes; no agent following `bd ready` can dispatch the workflow early.
- **A release run is trackable work**: claimable steps, notes as evidence,
  a digest at the end — instead of an unrecorded terminal session.
- **First trial at the next release** (acetone-bnn): instantiate with
  `bd mol wisp release --var version=<v>`. This ADR records an experiment; if
  the ceremony outweighs the value in practice, the formula is deleted as
  easily as it was added.
- **Two runtime caveats to re-test on bd upgrades**: pattern enforcement at
  cook/pour time, and gate auto-wiring (verified parsed via
  `bd formula show release --json`; unknown TOML keys are dropped silently).
- **Stretch not taken here**: a second `phase-boundary` formula (last bead →
  security review → phase report → demo deck → Greg's gate bead) is left for a
  follow-up bead once this one has been trialled on a real release.
