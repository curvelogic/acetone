# Phase 0.3.1 report — quality, security, and the manual

*Epics `acetone-7qw` (0.3.x quality & security pass) + `acetone-4zy` (operator's manual) · base `main @ 30db4d1` (v0.3.0) · this report covers the phase through `acetone-w9uu` (PR #194)*

0.3.1 is not a feature release. It is the release where the workbench is made
**trustworthy to hand to someone else**: hardened against hostile input,
honest about what it enforces, and documented well enough that an operator can
run it — and recover it — from the book rather than the source. It began as a
backlog-triage decision at the Phase 8 boundary (fold the two standing epics,
quality/security `acetone-7qw` and the manual `acetone-4zy`, into one release
before Phase 9's scale-and-conformance work) and grew as the manual's own
verification pass surfaced real bugs the code review had not.

No on-disk format change: the format stays at `format_version 1`, and 0.1–0.3.0
repositories are read and written unchanged (confirmed by the milestone
security review — no `format_version` bump, no golden re-bless in the whole
diff). openCypher TCK conformance rises to **1602 / 3897 (41.1%)** from 1596 at
0.3.0.

Everything landed autonomously under the usual gate: per unit of work a design
recorded in the bead, TDD, a fresh strongest-tier adversarial review with no
implementation context, fix/re-review, squash-merge on green CI, close. 34 PRs
merged (#160–#194, less the withdrawn #181); every code PR took a full
adversarial review, every governing-doc PR the full path plus an ADR, every
manual chapter a fresh-subagent doc review against the real binary.

## What shipped

### The operator's manual (`acetone-4zy`)

An mdBook at `docs/manual/`, published to GitHub Pages, whose every command and
output is driven against the real CLI — and a CI job that keeps it that way.

| Bead | PR | What |
|------|----|------|
| `4zy.1` | #162 | mdBook scaffold + GitHub Pages workflow (ADR-0047), SHA-pinned node24 actions, sha256-verified mdbook. |
| `4zy.2` | #167 | Part I — installing, first graph, and the **asset-registry worked example** (a committed, runnable setup script the rest of the manual reuses). |
| `4zy.3` | #173 | Part II — a ~40-recipe Cypher **query cookbook**; unsupported openCypher declined honestly with working alternatives. |
| `4zy.4` | #175 | Part II — **importing** end to end, with committed data files and real failure transcripts. |
| `4zy.5` | #178 | Part II — history/branch/merge (a real conflicted merge and repair), schema/indexes, maintenance/migration. |
| `4zy.6` | #180 | Part III — a **recovery runbook**: ten failure scenarios each broken and recovered for real. |
| `4zy.7`, `lk1`, `g1b` | #177 | Parts IV–V — library API (a compiled example), full CLI reference, conformance, glossary; `--json` stability settled as explicitly unstable pre-1.0; conformance count refreshed. |
| `4zy.8`, `20a` | #186 | Docs CI — `verify.sh` runs the manual's load-bearing examples against the binary; lychee link-checking. |

The manual paid for itself: its verification passes found the import
constraint bypass (`9gw`), the missing graph-violation surfacing (`jm8`), the
AT tag-resolution gap (`lqq`), the first-parent `log` blind spot (`b6q`), the
`--param` gap (`9zt`), and the branch-UX gap (`7qw.1`) — all fixed this phase.

### Security & correctness hardening (`acetone-7qw`)

| Bead | PR | What |
|------|----|------|
| `c2a`, `dfh` | #164 | **gc hardening** — a proven-live pack-sidecar **path traversal** closed; `owns_ref` narrowed to an enumerated allow-list (co-tenant ref-ownership); marker-name revalidation; a worktree-add TOCTOU closed under the writer lock. |
| `8t3`, `5lo`, `7fe` | #165 | **fsck** peels annotated tags and walks symbolic refs (rather than aborting), and dedups shared chunk sets across history. |
| `7vw`, `8gp`, `093` | #172 | Persist/encoding guards — non-round-trippable (bytes/temporal) **key values rejected**; CBOR preallocation **capped** (memory-amplification); Gate-D freeze-audit tests. |
| `596`, `0ds`, `6tt` | #171 | CLI output hardening — **zero-width/invisible Unicode escaped** on identifier output (incl. identifiers projected into result cells), completing the 0.1.1 bidi defence; blame key-arity error; lock-holder sanitisation. |
| `18z`, `bzr`, `v3k`, `5xp` | #176 | Executor resource bounds — var-length expansion, aggregation grouping, and `replace()` amplification charged against the budget; value-render depth guarded. |
| `19x` | #188 | **Fixed a reachable process-abort DoS**: a `reduce`-built 200k-deep value crashed `DISTINCT`/`ORDER BY`/grouping with a stack overflow; runtime value construction is now depth-capped, and query parameters bounded at ingestion (#189). |
| `9gw` | #184 | **Constraints enforced on every write surface** — import, put-node, and declare-retrofit now run the same existence/UNIQUE checks as a Cypher write (a confirmed UNIQUE **bypass** closed); fsck reports pre-existing breaches as advisories. |
| `q9m` | #174 | MERGE-relationship `SET = <entity>` / `ON CREATE` gaps closed (+4 TCK, 1598→1602); a TCK-harness control-query mis-reduction fixed. |
| `4lh`, `dm3` | #163 | Lexer accepts `i64::MIN` and hex/octal forms; clearer bare-hash AT errors. |
| `5fh`, `mqz`, `v8g` | #166 | Broadened merge test coverage incl. an indexed `merge == reindex` proptest (Invariant #5). |
| `ejj` | #179 | **migrate** rewrites annotated tags and swings every ref in one journalled, crash-recoverable transaction; signed tags refused rather than silently invalidated. |
| `tqd`, `ayq`, `k78` | #168 | Repository lifecycle hardening (ADR-0056) — recoverable checkout, **read-only open**, no-change commit refusal. |
| `jm8` | #187 | **Graph violations surface through the whole resolution flow** (ADR-0058) — live re-derivation, named refusal, a `kind` column on `acetone.conflicts`. |
| `3gy` | #191 | Investigated a reported writer-lock bypass — **premise false** (the observation was a no-op save that writes no ref); locking made precise in docs, pinned by tests. |
| `cbl.3` | #192 | Error-message quality pass — declare-first hint, `(no columns)` suppression, named map-projection error, duplicated-cause fix. |

### CLI & UX

| Bead | PR | What |
|------|----|------|
| `9zt` | #189 | `acetone query --param KEY=VALUE` — strict Cypher-literal binding (no string substitution), shell `:param`, composes with `--at`. |
| `lqq` | #185 | `AT`/`--at` resolve short tag names and peel annotated tags (git-parity precedence). |
| `b6q`, `7qw.1` | #190 | `log --all` (unforgeable structural merge line); `branch NAME [REFSPEC]` and `branch --delete`. |

### Process, governance & release machinery

| Bead | PR | What |
|------|----|------|
| `feh` | #160 | Roadmap-tail refresh — closed decision gates recorded, crates.io hold (ADR-0055). |
| `ka1`, `cbl.8` | #161 | CI actions off deprecated Node 20; main runs no longer self-cancel. |
| `sdg`, `cbl.9` | #169 | Spike gix feature-minimised (dropped the last MPL-2.0 resolution); `.beads/interactions.jsonl` de-tracked. |
| `074`, `ux4` | #170 | **Homebrew-tap auto-bump on publish** + SLSA **build-provenance attestation** on release archives. |
| `2vn`, `a8d` | #182 | Governing-doc corrections — CLAUDE.md crate list (acetone-core), spec constraint-timing wording. |
| `bxv` | #183 | The release process **encoded as a beads formula** (`.beads/formulas/release.formula.toml`, ADR-0057); trialled live at 0.3.1 prep. |
| `bnn` | #193 | 0.3.1 release prep — version bump, CHANGELOG cut, conformance refresh; the release formula's first live instantiation. |

## ADRs taken this phase

- **ADR-0055** — roadmap-tail refresh (governing doc; closed gates + crates.io hold recorded).
- **ADR-0056** — repository lifecycle hardening (recoverable checkout, read-only open, no-change-commit guard).
- **ADR-0057** — release process as a beads formula (`.beads/formulas/`; **flagged for governance ratification**, below).
- **ADR-0058** — graph violations re-derived live and named at completion.

## Milestone security review

A dedicated fresh-subagent security review swept the whole phase diff
(`v0.3.0..HEAD`) — untrusted-data panics/decode paths, path/ref injection, the
two GitHub workflows, terminal-output spoofing, constraint-enforcement
completeness, dependency risk, and format stability — with focused sub-audits
of the migrate/journal path, the executor recursion/depth cap, and the new
CLI surfaces.

**Verdict: gate clean.** No blocker- or high-severity findings, and the one Medium was fixed and merged in-phase (below).
`cargo audit` clean (0 advisories), no net-new workspace dependencies, no
format-byte change. Findings:

- **`acetone-w9uu` (Medium, reproduced)** — `migrate`'s crash-recovery applied
  journal ref-swings validated only for ref-*format*, not namespace
  *ownership*, so a crafted journal in a co-tenant repo could repoint the
  user's code branch. Reachable only via an attacker-supplied on-disk repo
  (not through clone/fetch), and no escape in standalone mode. **Fixed and merged in-phase** (PR #194, adversarial review confirmed both reproductions red on reverted code) rather than crossed to the boundary, per ADR-0054.
- **`acetone-becn` (Low/info)** — fsck names identifier-shaped tokens through
  the sentence-output path, so zero-width chars survive in fsck output
  (reordering/ANSI already neutralised). Accepted residual, optional hardening;
  filed.
- Rekey does not call the shared constraint checker (informational — safe by
  construction, no violation reachable; recorded for the enforcement map).

## Open risks & follow-ups crossing the boundary

Under ADR-0054 the phase resolves its own follow-ups by default, and it did —
every bug the manual surfaced was fixed in-phase, and the one Medium security
finding is being fixed rather than deferred. The items below genuinely belong
to later work or need Greg:

- **`acetone-63m.12`** (file the gix ref-transaction TOCTOU upstream) — an
  outward-facing action awaiting Greg's go-ahead; carried since Phase 8.
- Phase 9 (`acetone-2ck`) inherited, with reasons: `fht` (make `GraphError`
  `#[non_exhaustive]` — a break, best at a minor boundary), `42d` (co-tenant
  shared-worktree workspace-ref scoping — needs the co-tenancy design, and the
  #191 review's shared-HEAD observation joins it), `omk` (workspace
  discard/restore — a feature), `h7j3` (does null satisfy `REQUIRE` — a
  semantics decision). These are re-homed to the Phase 9 epic, not left under
  the closed phase.

## For Greg at the boundary

**Decisions queued (the ADRs are the agenda):**

1. **Ratify `.beads/formulas/` into CLAUDE.md's governing-config enumeration.**
   ADR-0057 treats the release formula as governing-config (full review + ADR),
   self-applying the stricter path; CLAUDE.md's enumerated list
   (`.beads/hooks/`, `.codex/`, `.claude/settings.json`) does not yet name it.
   This is a governance change — proposed here, made only if you agree.
2. **Close the two roadmap gates** if the evidence holds: the manual (Gate — docs)
   and the TCK conformance bar for this release.

**Actions that are yours (config that touches your machine/accounts):**

1. **`HOMEBREW_TAP_TOKEN`** — the auto-bump workflow (#170) needs a repo secret:
   a fine-grained PAT restricted to `curvelogic/homebrew-tap`, Contents
   read+write and Pull requests read+write. Until it exists the tap step fails
   loudly (everything earlier is read-only).
2. **Enable GitHub Pages** — Settings → Pages → Source = GitHub Actions, so the
   manual's deploy job publishes.

**The release itself:** 0.3.1 prep has landed on `main`. The Release workflow is
dispatched from there (nothing is tagged before you publish); **publishing the
draft is your human gate** — it creates the `v0.3.1` tag as you, and triggers
the Homebrew bump. The flow is available as a tracked molecule
(`bd mol wisp release --var version=0.3.1`).

## Process notes

- **The manual as a bug-finder.** Six real bugs came out of driving the CLI to
  document it, not out of code review — the strongest argument yet for
  documentation-by-execution. The verify-against-the-binary discipline is now
  enforced in CI (`verify.sh`).
- **Cross-PR semantic conflicts.** The no-change-commit guard (`k78`, #168)
  broke property-test harnesses on three later branches that committed
  deliberately-empty bases; each was resolved by opting into
  `commit_allow_empty`. Worth remembering when a behavioural default changes
  mid-phase: grep the test suite for the old assumption.
- **A dropped merge, caught late.** PR #174's first auto-merge silently failed
  on a transient status; the security review's TCK reconciliation surfaced it — main sat at
  1598 (1596 at 0.3.0, plus the +2 from the i64::MIN lexer fix in #163),
  where #174's MERGE-relationship work should have taken it to 1602. Auto-merge state deserves an
  explicit post-check.
