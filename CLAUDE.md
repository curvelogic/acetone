# Project Instructions for AI Agents

This file provides instructions and context for AI coding agents working on this project.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:6cd5cc61 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->


## What Acetone Is

Acetone is an embedded, single-node, **version-controlled labelled property graph database**: Dolt-style prolly trees stored in a git-compatible object store, queried with openCypher, operated as a workbench (CLI + Rust library, not a server). Written entirely in Rust.

The design record lives in `docs/` and is authoritative:

- `docs/acetone-01-design-space.md` — vision, prior art, and the six shaping decisions
- `docs/acetone-02-spec.md` — the v0.1 specification (data model, storage, encodings, query language, diff/merge, CLI)
- `docs/acetone-03-roadmap.md` — phased implementation plan (Phase 0–6) with exit criteria and decision gates

Read the spec before implementing anything; when code and spec diverge, either fix the code or update the spec deliberately — never silently.

## Architecture Overview

Cargo workspace of six crates with strictly downward dependencies:

```
acetone-cli     — thin CLI client
acetone-cypher  — parser front end, binder, planner, iterator-model executor, TCK harness
acetone-graph   — graph mutations, constraints, validation, merge orchestration
acetone-model   — node/edge keys, records, order-preserving encodings, schema, manifest
acetone-prolly  — prolly trees: build, scan, diff, three-way merge
acetone-store   — ChunkStore trait; git object database backend (gitoxide, git2 fallback); refs/commits
```

Plus `tck/` (vendored openCypher TCK runner) and `benches/` (Phase 0 benchmarks kept as regressions).

## Load-Bearing Invariants

These are normative and enforced by property tests — breaking any of them is a format/design bug, not a refactor:

1. **History independence**: identical map contents MUST yield identical prolly-tree root hashes regardless of operation order.
2. **Deterministic encodings**: memcomparable key encoding (byte order == logical order) and canonical deterministic CBOR values. Any change bumps `format_version`.
3. **Node identity is `(primary label, key tuple)`** — natural keys mandatory, declared in schema. `SET` must never modify key properties.
4. **Merge determinism**: `merge(base, ours, theirs)` is a pure function; conflicts are data (the `conflicts` map), not errors.
5. **Derived maps** (`edges_rev`, indexes) must be exactly reproducible from their sources (`reindex` yields identical roots).

## Build & Test

```bash
cargo build --workspace
cargo test --workspace          # includes property tests (proptest)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Test-driven throughout: property tests for storage invariants land before or with the features they guard; TCK conformance drives the Cypher engine and the pass rate is published per release.

## Autonomous Working Protocol

Greg reviews **only at phase boundaries**. Between boundaries, agents work autonomously under these rules. This repository grants agents the rights of the beads **Team-maintainer** profile (close beads, run quality gates, commit, push) and additionally the right to **merge PRs**, subject to the merge gate below. The beads-managed block above stays at its minimal/pointer profile; this section is the "active instructions say otherwise" that it defers to.

Per unit of work (one bead, or a small coherent group claimed together — the group then counts as one unit):

1. **Claim**: `bd ready` → `bd update <id> --claim`. One unit of work in flight per agent.
2. **Spec first**: before coding, record the approach in the bead (`bd update <id> --design`): interfaces, key types, test plan, and how the work protects the Load-Bearing Invariants. Non-trivial design choices get an ADR (see below).
3. **TDD**: tests precede or accompany implementation; invariant-touching code carries property tests. No `unsafe` without justification and a test.
4. **Branch & PR** per the branch discipline. CI (build, test, clippy `-D warnings`, fmt, audit) must be green.
5. **Independent review — mandatory merge gate**: every PR is reviewed by a **fresh subagent with no implementation context**, prompted to review adversarially for correctness, security, spec conformance (docs/acetone-02-spec.md) and invariant protection. The gate is **reviewer sign-off, not implementer self-certification**: each finding is fixed or rebutted, the responses go back to a reviewer (the same one, or a fresh one given the full finding/response trail), and the PR merges only when the reviewer accepts. Non-trivial fix commits are re-reviewed. **On unresolved disagreement the PR does not merge**: file a `decision` bead, leave the PR open, flag it in the phase report for Greg.
6. **Merge & close**: squash-merge once CI is green and the reviewer has signed off; delete the branch; close the bead with a one-paragraph summary of what shipped and any deviations from the spec'd design.

Review depth by change class:

- **Code**: full adversarial review as above, always.
- **Governing documents** (CLAUDE.md, `docs/acetone-01/02/03-*.md`, and executable configuration that runs on Greg's machine: `.beads/hooks/`, `.codex/`, `.claude/settings.json` — added at the Phase 0 boundary with Greg's approval): full adversarial review — never the lighter path — plus an ADR, listed prominently in the next phase report. Agents must never weaken the review gate or expand their own merge rights; such changes are proposed in the phase report and made only after Greg agrees.
- **Other docs** (notes, reports, ADR text): lighter single-pass review, still by a fresh subagent, checking factual accuracy and consistency with the design record.

Subagent model tiers (ADR-0009, Greg-approved 2026-07-04): match the model tier of a dispatched subagent to the cost of an undetected error, not to the task's prestige. Tiers are relative to whatever models the session offers — model names change; the tiers are the policy.

- **Strongest available tier**: adversarial PR reviews and milestone security reviews. The review gate is never downgraded to save tokens, at least until Gate D (format freeze); revisit after.
- **Mid tier**: design/planning subagents and lighter-path doc reviews — escalating to the strongest tier when the work touches Load-Bearing Invariants or on-disk format.
- **Smallest tier**: exploration/search fan-outs and mechanical batch work, where output is cheap to verify by use.

Additional gates:

- **Milestone security review**: at each phase's end, run a dedicated security-focused review (fresh subagent) over the whole phase diff — input handling, path/ref injection, panics on untrusted data, dependency risk — before writing the phase report. Findings are triaged like PR findings: fixed, or filed as beads; unresolved blocker-class findings are listed as open risks and the report must then say the gate is **not** ready to close.
- **Decisions**: any choice Greg would plausibly care about (dependency adoption, format details, API shape, spec deviations) is recorded as `docs/adr/NNNN-<slug>.md` (context → decision → consequences, a page at most) and linked from a `decision`-type bead. Decisions are **made, not deferred** — ADRs are the agenda for phase-boundary discussion, not permission requests.
- **Roadmap decision gates**: gates coinciding with a phase boundary (A — git-as-chunk-store; C — TCK bar) are Greg's call, evidenced in the phase report and settled when he closes the gate bead. Gates falling mid-phase (B — parser adoption; D — format freeze) are decided by ADR so work proceeds, and flagged prominently in that phase's report for retrospective review.
- **Phase boundary**: agents never close the bead that gates the next phase (the exit-criteria bead; for Phase 0, the Gate A decision bead). The agent completing the phase's last working bead writes `docs/reports/phase-N.md` — what shipped, gate evidence against the roadmap's exit criteria, ADRs taken, review findings summary, open risks — and stops. The boundary review then runs as a **sprint demo** (Greg's preferred format, set 2026-07-04): (1) a several-slide presentation (Artifact, archived to `docs/demos/`) covering context, where we are, what was done, and the problems and decisions that arose; (2) a live demo — driving the phase's actual code/CLI/tooling **step by step, one step per turn**: announce the step, run it, quote the command and output in the message text, explain what it means, and stop for discussion before proceeding (never batch steps into background calls or one summary); (3) Greg reviews the docs in detail at his own pace. Greg then discusses, rules on queued decisions, and closes the gate bead; the next phase's beads unblock from there.
- **Dependencies**: adding a crate requires a sentence of justification in the PR (maintenance health, licence, why not std/existing deps). `cargo audit`/`cargo deny` run in CI.

## Branch & Merge Discipline

Trunk-based development with short-lived branches:

- **`main` is always green**: builds, `cargo test --workspace`, clippy at `-D warnings` and `fmt --check` must pass on every commit that lands there. Never commit directly to `main`; never force-push it.
- **One bead per branch**, named `<bead-id>/<short-slug>` (e.g. `acetone-28x.1/scaffold-workspace`). A branch may cover a small coherent group of beads when they only make sense together — say so in the PR description.
- **Merge via PR, squash-merged**, so `main` stays linear and one commit ≈ one bead. Delete the branch after merge.
- **PR title = squash commit subject**: imperative mood, ≤ 72 chars, referencing the bead, e.g. `Scaffold cargo workspace and CI (acetone-28x.1)`. The PR body carries the detail; close the bead when the PR merges.
- **Rebase on `main`** to update a branch; never merge `main` into a feature branch.
- **Phase exit criteria are review points**: the exit-criteria bead of each phase is closed by a human (Greg) after reviewing the gate evidence, not by an agent.
- `.beads/` sync is bd's business (`refs/dolt/data`); don't hand-edit or commit beads data through ordinary git operations.

## Conventions & Patterns

- Rust 2024 edition; `rustfmt` defaults; clippy clean at `-D warnings`.
- Errors via `thiserror` in library crates; `anyhow` only in `acetone-cli`.
- No `unsafe` without a comment justifying it and a test exercising it.
- UK English in docs and user-facing text.
- Work is tracked in beads: phases from the roadmap are epics; issues carry dependencies mirroring the phase gates. Check `bd ready` before starting anything.
- Environment facts that shape design or workflow live in `docs/notes/operational-constraints.md` (e.g. git proxies may 403 non-standard ref namespaces — transferable state belongs in `refs/heads|tags` only).
- Process learnings and working agreements are recorded here or in `docs/` through reviewed PRs — never only in agent-local memory, which the repo and fresh agents cannot see.
