# Phase 6 report — hardening towards 0.1

*Prepared at the Phase 6 boundary for the sprint-demo review. Phase 6 is the
epic `acetone-cbl`. `main` is green at `3f59eba`.*

Phase 6 turns acetone from a feature-complete workbench into one you can **rely
on and ship**. Its through-line is *format stability*: name the on-disk layout
for co-tenancy, freeze it, and build the escape hatch that makes freezing safe —
then package the result and write it down. A repository written by this release
will be readable by the next, and where the format must change, `acetone
migrate` carries history forward.

## What shipped

Five PRs (#73–#77), each behind a fresh-subagent adversarial review:

- **`.acetone/` tree namespacing** (acetone-gbd, #73, ADR-0023). The
  machine-readable commit- and workspace-tree entries — the `manifest` blob and
  the `chunks/` anchor tree — moved from the tree root into a reserved
  `.acetone/` directory, with `README.md` kept at the root for hosting-UI
  auto-render. Envelope-only: commit/workspace-tree OIDs change, but manifest,
  chunk and prolly map-root hashes do not. Landed *before* the freeze so the
  format ships already-namespaced and never needs migrating for co-tenancy.
- **Gate D — format freeze** (acetone-cbl.1, #74, ADR-0024). Three fresh,
  adversarial format-freeze audits (one per encoding surface — keys/records,
  value CBOR, schema + prolly + chunking) all returned *freeze-ready-with-nits*.
  `format_version = 1` is **frozen, unchanged in bytes**. The one real gap the
  audit found — the prolly node/chunker format was guarded only by *self-relative*
  property tests and could drift silently — is closed by golden byte- and
  root-hash pins (`crates/acetone-prolly/tests/golden.rs`). Spec §3.4 was
  corrected ("order-preserving chunked framing", not the order-breaking
  "length-framed UTF-8") and its deliberate scalar-domain caps ratified
  (`DateTime` i64-nanosecond range ≈ 1677–2262, minute-granular UTC offset).
- **`acetone migrate` — history-rewrite engine** (acetone-hsg, #75, ADR-0025).
  The freeze's escape hatch: a generic engine that re-encodes every reachable
  version under a `FormatTransform` and rebuilds the commit graph with new
  hashes, **preserving each commit's message, author and committer — identity
  and timestamp — verbatim** (a new store `rewrite_commit` metadata path). Per
  the Gate D decision ("engine now, demo at 0.2"), the shipped transform is a
  version-preserving re-chunk that exercises the full engine; a real
  cross-version transform slots in at the first 0.2 bump. Deterministic and
  idempotent; `git fsck` stays clean.
- **`acetone-core` façade + release packaging** (acetone-cbl.5 + cbl.7, #76,
  ADR-0026). `acetone-core` is introduced as the library product surface (spec
  §7), resolving the §7/§8 inconsistency by making §8 list it. A
  `[profile.release]` and a `Release` workflow build the single static binary
  (musl on Linux, platform binary on macOS) on a `v*` tag; the workspace is
  publish-ready (leaf `acetone-store` dry-runs clean; bottom-up publish
  documented in `RELEASING.md`).
- **User guide + conformance statement** (acetone-cbl.4, #77). `docs/user-guide.md`
  (the everyday CLI + library workflow) and `docs/conformance.md` (the published
  openCypher pass rate and known-gap list), plus a root `README.md` and the
  roadmap Phase 6 status. Delivers the "publish the conformance statement" half
  of acetone-cbl.2.

## Gate evidence — roadmap Phase 6 exit criteria

The roadmap's Phase 6 exit is *"0.1 tagged; a fortnight of dogfooding without
data-integrity incidents; the three documents in this pack revised to match
reality."*

- **Three documents revised to match reality** — ✅. The spec (02) was kept
  current through the phase (§3.4 encodings + frozen scalar domains, §3.5
  `.acetone/` layout + freeze, §8 crate layout with `acetone-core`); the roadmap
  (03) carries a Phase 6 status note; the design-space (01) is vision and stands.
  New user-facing docs (user guide, conformance statement, RELEASING) shipped.
- **0.1 tagged** — a human call at this boundary (the release workflow and
  publish process are ready; tagging reserves crates.io names irreversibly).
- **A fortnight of dogfooding** — inherently post-boundary; the packaging that
  unblocks it (cbl.5) shipped, and the dogfood run itself (cbl.6) is Greg's
  real asset-registry on his private remote.

**Gate D (format freeze), the phase's mid-phase decision gate**, is evidenced by
ADR-0024 and the golden anchors: every on-disk artefact — keys, values, records,
schema, manifest, **and now the prolly node/chunker format** — is byte-pinned, so
a format change fails a pin loudly rather than drifting silently, and the
manifest's `format_version` is checked first on every decode.

## Decisions taken (ADRs)

- **ADR-0023** — namespace machine entries under `.acetone/`, README at root.
- **ADR-0024** — freeze `format_version = 1`; **flagged**: the schema index
  entry is frozen as `{label, property}` (not speculatively widened for
  composite indexes) — the one Gate D choice a reviewer might make differently,
  surfaced here for retrospective ratification.
- **ADR-0025** — the migrate engine; **flagged**: "engine now, demo at 0.2" (a
  Greg decision at the Gate D boundary) — the cross-version demonstration is
  deferred to the first real format bump rather than built against a synthetic
  format.
- **ADR-0026** — the `acetone-core` façade and packaging.

Gate D and the migrate demo-scope are *mid-phase gates decided by ADR so work
proceeds*, flagged here for retrospective review per CLAUDE.md.

## Review findings summary

Every PR drew a fresh-subagent adversarial review, and the gate earned its
keep — a real issue on nearly every one:

- #73 `.acetone/`: APPROVE-WITH-NITS (missed CI-script and workspace-ref tree
  paths caught by CI, then fixed).
- #74 Gate D: three opus freeze-audits found the prolly golden-pin gap (fixed
  before freeze) and the §3.4 wording trap; PR review independently recomputed
  the git-blob hash and the DateTime-range arithmetic. APPROVE-WITH-NITS.
- #75 migrate: review found a **missing merge-commit test** (the engine handled
  merges — verified externally — but it was unproven in-tree; a two-parent
  migration test was added) and the crash-mid-migration recovery caveat.
  CHANGES-REQUESTED → APPROVE.
- #76 packaging: review found a **real command-injection** via `github.ref_name`
  in the release workflow (git tag names allow `$`/backticks; CI never runs the
  tag-triggered workflow, so it could not catch it) — fixed by routing through
  an `env:` var. CHANGES-REQUESTED → APPROVE.
- #77 docs: doc-accuracy review verified every CLI flag against the code and the
  conformance arithmetic; three citation nits fixed. APPROVE-WITH-NITS.

A CI toolchain drift (clippy 1.97 tightened `question_mark`/`unneeded_wildcard_pattern`
and flagged pre-existing code) was fixed across `tck` and two test files to keep
main green.

## Milestone security review

A dedicated fresh-subagent security pass over the whole Phase 6 diff
(`30356b4..HEAD`) — input handling, panics on untrusted data, path/ref
injection, dependency risk — returned **GATE READY**. No blocker/high/medium
findings.

- The previously-flagged **release-workflow tag injection is confirmed closed**
  (`github.ref_name` is consumed only as a bash `${REF_NAME}` env var; the
  remaining `matrix.target` interpolations are trusted workflow literals);
  actions are SHA-pinned, `permissions` are minimal, and no secret is exposed
  (publishing is manual).
- Every new untrusted-read path returns a **typed error under a size cap, never
  a panic**: the `.acetone/` reader (`root_manifest_hash`) requires the right
  object kinds and works purely by OID (no filesystem path, so no traversal);
  `read_commit`'s new identity/timestamp parsing fails safe; the migrate walk's
  memory is input-proportional and bounded by the object-size cap; and
  `rewrite_commit` writes the message in the body only and **validates author/
  committer identities** (rejecting `<`/`>`/newline), closing any
  commit-header/trailer-injection vector.
- **No new dependencies** entered the tree (`Cargo.lock` adds only the internal
  `acetone-core` façade); `cargo migrate` params are validated by
  `ChunkParams::new`, rejecting pathological values.

Three INFO-level robustness notes were recorded (migrate fails closed on
git-illegal identities; `read_commit` is now stricter on malformed timestamps;
a journaled/streaming migrate walk is the tracked scalability follow-up) — none
is a blocker.

## Open risks and deferred work

Filed as beads; none blocks the boundary:

- **Dogfood in anger** (acetone-cbl.6) — the flagship, inherently a post-boundary
  fortnight on Greg's real private remote; packaging is ready.
- **TCK pass-rate improvement** (acetone-cbl.2) — the conformance statement is
  published (41.0%); the numeric climb is gated on parser-feature beads with no
  cheap wins: `acetone-4lh` (`i64::MIN` literal), `acetone-cxh` (pattern
  comprehension), `acetone-6gy` (label predicate), `acetone-q9m` (MERGE-rel),
  `acetone-i8z` (CALL shapes).
- **Error-message quality pass** (acetone-cbl.3) — spans/suggestions/vocabulary
  across parser/binder/constraints/merge; a 0.1-polish sweep, not a blocker.
- **Store-backed lazy IndexSeek** (acetone-cbl.11) — the scalability win over
  the in-memory seek (ADR-0022); a perf optimisation, correctness-sensitive.
- **migrate hardening** — annotated-tag rewriting + atomic multi-ref swing;
  **CLI through the `acetone-core` façade** and a **library-level query entry
  point**; **history-retention strategy** (acetone-cbl.10). All filed.

## The demo

The live demo drives the phase's own tooling, step by step: freeze evidence (a
golden pin failing loudly under a simulated format change), `acetone migrate`
rewriting a small history with preserved authorship and a clean `fsck`, and the
release/packaging surface (`acetone-core` doctest, the leaf publish dry-run, the
release-profile binary). See the sprint deck (`docs/demos/`).
