# ADR-0047: Deliver the operator's manual as an mdBook published to GitHub Pages

*Status: proposed — brainstormed with Greg 2026-07-20; pending his review of this ADR and the epic design (`acetone-4zy`) · Date: 2026-07-20 · Bead: acetone-4zy.1 · Relates: ADR-0026 (packaging), ADR-0046 (frozen 0.2 API — the rustdoc seam), `docs/notes/operational-constraints.md`*

## Context

Through 0.2 the documentation is a growing pile of authoritative markdown:
`docs/user-guide.md` (a 146-line *tour*), `docs/conformance.md`, the three
design-record documents, 47 ADRs, and the phase reports. GitHub renders each
file, but there is no navigable, searchable, operator-facing **manual** — and
the highest-value operator content (worked Cypher examples, an end-to-end import
walkthrough, and failure/recovery procedures) barely exists.

A brainstorm with Greg (2026-07-20) fixed the intent: the primary audience is
**operators** running acetone in anger against the dogfood asset-registry, not
external contributors or a marketing front; both the *content* and its
*delivery* are gaps; and we write the full manual in this pass rather than
scaffold-only. Greg raised GitHub Pages (as used in his eucalypt project) versus
alternatives.

Constraints that shape the mechanism:

- The environment **403s pushes to non-standard ref namespaces**
  (`docs/notes/operational-constraints.md`), so any delivery that publishes by
  pushing a `gh-pages` branch is fragile here.
- `main` must stay green; docs should be a first-class CI gate, not drift.
- Doc source must stay **reviewable markdown** so it rides the same
  fresh-subagent PR discipline as the rest of the repo, with no JS toolchain for
  an agent to fight.
- The design record is authoritative and separately reviewed; the manual must
  **not** duplicate it (that would reintroduce the exact spec-drift ADR-0031
  fought).

Alternatives weighed: **(A) mdBook → Pages**; **(B)** better-organised
GitHub-rendered markdown (no search, no unified manual); **(C)** a full
static-site generator (Zola/Hugo/Docusaurus — heavier, partly non-Rust
toolchain, more than operator-first needs now).

## Decision

Adopt **mdBook published to GitHub Pages** (option A).

1. **Source layout.** mdBook root at `docs/manual/` — `book.toml`,
   `src/SUMMARY.md`, one markdown file per chapter. Build output
   `docs/manual/book/` is gitignored and built only in CI. The design record
   (`docs/acetone-0{1,2,3}`, `docs/adr/`, `docs/reports/`) stays where it is and
   is **linked, never embedded**.

2. **Pipeline — Actions-native Pages, no `gh-pages` branch.**
   `.github/workflows/docs.yml`: on push to `main`, `mdbook build` then deploy
   via `actions/upload-pages-artifact` + `actions/deploy-pages`, which publishes
   through the Pages API rather than by pushing a branch. This sidesteps the
   custom-ref-namespace 403 and keeps `main` linear.

3. **`main` stays green.** On pull requests the workflow runs `mdbook build`
   (optionally `mdbook-linkcheck`) as a **build-check only, no deploy** — a
   broken manual or a dead internal link fails CI like any other regression.

4. **Dependency.** `mdbook` is a **CI-only tool binary** (rust-lang project,
   MPL-2.0, actively maintained) installed in the workflow. It never enters
   `Cargo.toml`, so it does not touch the workspace crates or the
   `cargo audit`/`cargo deny` surface. An optional `mdbook-linkcheck` is the same
   class.

5. **rustdoc seam, deferred.** The library chapter links to the frozen 0.2
   surface (`STABILITY.md`) for now. It becomes a docs.rs link or a second
   rustdoc build step **only once the crates.io publication call is settled** (a
   separate roadmap-review item). No work is committed to that here.

6. **Authoring rule.** Because acetone is at 41% TCK, **every Cypher/CLI example
   is executed against the acetone binary while authoring**; the manual documents
   only what works, noting and cross-linking gaps where a natural query is not yet
   supported. A follow-on "doctest the manual" bead (`acetone-4zy.8`) enforces
   this in CI later.

7. **Review classing.** The pipeline PR (workflow + scaffold) is **code class**
   → full fresh-subagent adversarial review, and carries this ADR. The prose
   chapters are **other-docs class** → lighter fresh-subagent review for factual
   accuracy against the code and consistency with the design record.

One human step remains Greg's: enabling *Settings → Pages → Source = GitHub
Actions*. The workflow will not publish until that is on.

## Consequences

- Operators get a searchable, navigable manual with worked examples on a single
  running dataset; the content and delivery gaps close together.
- Doc source stays plain markdown under the normal PR gate — cheap for agents to
  maintain and for reviewers to read — and docs building becomes a CI gate.
- The design record is not copied, so there is a single source of truth; the
  manual points at it.
- Publishing via the Pages API avoids the known ref-namespace 403 and adds no
  library dependency and no non-Rust toolchain.
- One new CI-only binary dependency (`mdbook`) and one new workflow to maintain.
- The rustdoc/docs.rs half of "API documentation" is deliberately **not** solved
  here; it is gated on the still-open crates.io decision and named as a seam.
- If acetone later goes properly public (external users, versioned docs), option
  C (a full SSG) can be revisited; nothing here forecloses it.
