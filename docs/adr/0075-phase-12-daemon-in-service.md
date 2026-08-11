# ADR-0075: Phase 12 — the daemon in service

*Status: accepted — theme, membership and rulings confirmed by Greg at
the phase opening (2026-08-11): "open the next phase and get started
with implementation", with the two queued decisions ruled in the same
session (ADR-0076, ADR-0077) · Date: 2026-08-11 · Bead: acetone-zavr*

## Context

Phase 11 shipped the daemon, multi-graph co-tenancy and rel-type
rename/merge; 0.6.0 is released and the Phase 11 gate is closed. The
boundary assessment of the daemon against the CLI found it complete in
its core verbs but short of parity in specific, enumerable places, and
the 0.6 grooming session with Greg agreed a phase shape around closing
them (recorded on `acetone-zavr` 2026-08-11). Two decisions were queued
for the opening and are ruled now: the cross-language surface strategy
(ADR-0076: protocol-first) and stale-writer-lock robustness (ADR-0077:
enforce daemon-exclusivity at startup).

## Decision

Phase 12 is **"The daemon in service"** (size M, target version 0.7).
Three strands, in rough order:

1. **Daemon completion** — verb parity with the CLI: `status` frame
   parity (schema_entries, workspace, merge block — `acetone-sye1`);
   a schema-inspection and incremental declaration path over the socket
   (`schema show`, declare-label/-rel-type or a documented equivalent —
   `acetone-ezyj`); `params.at` on the query verb for whole-query time
   travel (`acetone-ghpf`); `export` and `fsck` verbs; graceful SIGTERM
   drain; the stdio transport (`acetone serve --stdio`, `acetone-zavr.2`,
   companion to ADR-0076); and the writer-lock unit (`acetone-pz0k.7`)
   scoped by ADR-0077 — startup exclusivity lock plus the deferred
   pid-reuse refinement and `schema-apply`/`import` recovery paths.
2. **History surfaced** — `log`/`blame` surfacing and structured
   change-report export (both tenant-pulled, ADR-0072 decision 4; the
   substrate shipped in earlier phases), plus ancestry refspecs
   (`main~1`, `HEAD^` — `acetone-bvq`, which composes with `params.at`),
   so that the tenant's reading-diffs feature is buildable entirely
   over the socket.
3. **Multi-graph residuals** — the per-graph worktree durability anchor
   (`acetone-j6ui.4`, the serious one: two prune-decision sites, its own
   reviewed unit), stale shared-workspace-ref cleanup (`acetone-j6ui.3`),
   and `check_path` fallback scoping (`acetone-j6ui.1`).

Optional quality garnish if size allows, explicitly not gate-bearing:
`EXPLAIN`/`PROFILE` (`acetone-a9m`) and workspace discard/restore
(`acetone-omk`).

**Exit criteria** (gate bead `acetone-zavr.3`, closed by Greg or by
explicit delegation per ADR-0067):

1. The boundary assessment's daemon gaps are closed through the shipped
   interface: `status` at CLI parity, a schema-inspection and
   declaration path, `params.at`, `export` and `fsck` verbs, SIGTERM
   drain, and the stdio transport.
2. A history-facing feature — reading diffs, blame and a structured
   change report — is buildable **entirely over the socket** by a
   non-Rust client, demonstrated end-to-end.
3. Writer-lock robustness per ADR-0077: two daemons on one repository
   are refused at startup, and stale-lock recovery covers all daemon
   write paths.
4. The multi-graph residuals (`acetone-j6ui.1/.3/.4`) are resolved.

## Consequences

- The roadmap gains a Phase 12 section; blame/log surfacing and
  structured change-report export leave the unscheduled list;
  `acetone-bvq` is re-homed into the phase.
- Beads are cut at opening for the members without handles (export/fsck
  verbs, SIGTERM drain, blame/log surfacing, change-report export) and
  the epic is retitled from its placeholder.
- The frame protocol's promotion to a documented versioned artefact
  (ADR-0076) rides on this phase's parity work — parity is what makes
  the document worth versioning.
