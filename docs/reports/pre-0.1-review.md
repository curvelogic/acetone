# Pre-0.1 comprehensive adversarial review

*Gate: roadmap "step 2.5" — the hard gate before tagging `v0.1.0`. Commissioned
by Greg (2026-07-10). Four fresh, adversarial reviewers on Fable 5, no
implementation context, one per dimension: architecture, code, security,
fitness/docs. Bead: `acetone-upq`.*

This report consolidates the four dimension reviews into one deduplicated,
severity-ranked, phase-categorised record. Every finding is either filed as a
bead here or already tracked; the bead IDs are given inline.

---

## 1. Verdict

**Tag 0.1 — after a pre-tag hardening sprint — as an honest, solo CLI
workbench; keep the library internal.** The frozen storage format is sound and
the freeze holds. But the review found 14 urgent correctness, data-loss and
crash defects (U1–U14 below) that a green CI and a clean clippy could not see,
and it explained why they cluster in one place.

The architecture reviewer's framing is the through-line for all four
dimensions:

> **acetone is two systems of different maturity wearing one name.** Below the
> manifest: a genuinely rigorous content-addressed storage engine —
> history-independent, golden-pinned, conflicts-as-data, exhaustively
> property-tested. Above it: a query/runtime layer that treats the frozen value
> domain as something to render into debug strings, edge identity as something
> to improvise per snapshot, and the whole version as something to load per
> statement. **Every urgent finding lives at that boundary** — where the storage
> layer's invariants stop being *enforced* and start being *assumed*.

The reassuring half is real and load-bearing: `acetone-prolly` and
`acetone-model` were read line by line and came back with no urgent defect
("some of the most disciplined storage code I've reviewed"); the hostile-repo
decode path is "exceptionally hardened" (security). The Load-Bearing Invariants
(history independence, deterministic encodings, node identity, merge
determinism, derived-map reproducibility) are intact in the core. **The Gate-D
freeze is not reopened by this review.**

What is *not* mature is the operational shell around that core and the seams
between Phase-4 and Phase-5 features — each of which was reviewed *within its
phase*, never against the running product. That seam is where 0.1 must be
hardened before it ships.

---

## 2. Urgent — must fix before the `v0.1.0` tag

These are correctness / data-loss / crash defects reachable from the 0.1 CLI
surface. They form the **pre-0.1 hardening sprint** (its own sprint demo before
the tag). Each is fixed under the Autonomous Protocol: TDD, fresh adversarial
review, merge.

| # | Defect | Where | Bead |
|---|--------|-------|------|
| U1 | **`gc` run twice truncates the live content-addressed pack in place** → repository destruction on crash between truncate and rewrite. Fix: no-op if `{stem}.pack`/`.idx` already present; write temp + atomic rename. | `acetone-store/consolidate.rs` (`install_pack`, `write_synced`) | `acetone-h4a` |
| U2 | **Cypher write-back silently retypes temporal and `Bytes` properties to strings.** The read adapter renders `DateTime`/`Date`/`Time`/`Duration`→debug strings and `Bytes`→hex; the write path replays the whole record, so `SET n.x=…` on any node bearing a temporal/bytes property rewrites that property as a `String`. Reachable in the solo CLI (e.g. `put-node` then any `SET`). Silent corruption of the frozen first-class value domain. | `acetone-cypher/exec/adapter.rs` (`convert_value`) | `acetone-2vr` |
| U3 | **Redeclaring a label's key tuple over live data corrupts identity**, unguarded. `declare-label` replaces a `LabelDef` with a different key while nodes exist under the old key → Invariant #3 violated silently. Fix: reject key-tuple change when data bearing the label exists (or require an explicit migrate). | `acetone-cli/commands.rs` (`declare_label`) | `acetone-9kp` |
| U4 | **`acetone shell` silently discards write queries.** `run_in_shell` executes reads and drops the write path — the user's `CREATE`/`SET`/`DELETE` appear to succeed and vanish. | `acetone-cli/query.rs` (`run_in_shell`) | `acetone-c8b` |
| U5 | **Merge completion skips graph validation** → dangling edges committed. `resolve_conflicts_from` / `commit` on merge completion do not call `validate_merged`, so a merge that leaves an edge with a missing endpoint commits a structurally invalid graph. | `acetone-graph/repo.rs` (`resolve_conflicts_from`, `commit`) | `acetone-3xd` |
| U6 | **Import commits dangling edges**, no endpoint check — an import whose edges reference absent nodes commits an invalid graph. | `acetone-graph/import.rs` | `acetone-3xd` (landed together) |
| U7 | **fsck has no referential-integrity check** — it cannot *detect* the U5/U6 dangling-edge class. Add an edge-endpoint-existence pass. Land with U5/U6 so the corruption is both prevented and detectable. Pairs with fsck-on-damaged-workspace (`acetone-zhp`). | `acetone-graph/fsck.rs` | `acetone-3xd` (grouped) |
| U8 | **Index redeclaration leaves a stale map** → silent wrong-empty seeks. Redeclaring an index does not rebuild/replace its `idx/<name>` map, so subsequent seeks read stale (often empty) results. | `acetone-graph/index.rs` | `acetone-fq2` |
| U9 | **Merge is impossible on any indexed repo.** `merge_manifests` returns `MergeUnsupported{"secondary indexes (arrive in Phase 5)"}` for any non-empty `indexes` map — a stale guard. Fix: rebuild indexes from merged `nodes` via `index::rebuild_all`, exactly as `edges_rev` is rebuilt (derived-map reproducibility, Invariant #5). | `acetone-graph/merge.rs` (`merge_manifests`) | `acetone-mk7` |
| U10 | **Edge property named `src`/`dst`/`disc` corrupts export.** In `edge_table` these property columns overwrite the endpoint/discriminator columns → wrong endpoints in exported CSV/JSON. Fix: namespace or reject the collision. | `acetone-cli/export.rs` (`edge_table`) | `acetone-wj4` |
| U11 | **`CREATE` of an existing (or parallel) edge silently upserts / collapses.** Edge key omits any discriminator (hardcoded `Null`, unreachable from the query surface), so two `CREATE`s of the "same" edge collapse and a second overwrites the first silently. | `acetone-cypher/persist.rs` (`edge_key`) | `acetone-8yn` |
| U12 | **Variable-length expansion recurses per hop** → stack-overflow abort (SIGABRT) on deep/cyclic paths from untrusted query input. Fix: iterative worklist with a bounded frontier. | `acetone-cypher/exec/run.rs` (`expand_var_length`) | `acetone-6qd` |
| U13 | **`range()` increments `at += step` unchecked** → i64 overflow panic and unbounded list growth → OOM. Fix: `checked_add` + a hard list-length cap (shared with the governor, `acetone-iq6`). | `acetone-cypher/exec/functions.rs` | `acetone-6qd` (grouped) |
| U14 | **`write_ref` value-equal create accepts a no-op** (gix `MustNotExist`), breaking the create-CAS contract. Now a trivial read-under-the-writer-lock fix. | `acetone-store/git.rs` (`write_ref`) | `acetone-0ej` (exists) |

**Pre-tag hardening (cheap, ship with the sprint):**

- **`#![forbid(unsafe_code)]`** on `acetone-model`, `acetone-cypher`, `acetone-cli`,
  `acetone-core` (the decode/query/CLI crates that touch untrusted input). Bead
  `acetone-uf0`.
- **Release trust**: tag-protection ruleset restricting `v*` tags to Greg;
  immutable releases; build attestation/provenance on the GHA artefacts;
  job-level (not workflow-level) `permissions`. Folded into `acetone-wpx`.

**Pre-tag doc / UX honesty sweep** (bead `acetone-do1`) — nothing an installer
reads in the first hour should be contradicted by the binary:

- `--version` reports `0.0.1` (workspace `Cargo.toml`); a `v0.1.0` tag must ship
  a `0.1.0` binary.
- `merge --help` / `query --help` self-contradictions; `:diff` shell command
  documented but absent.
- Commits are authored `acetone <acetone@acetone.invalid>` — git identity /
  `--author` ignored. At minimum honour git `user.name`/`user.email`.
- Time-travel docs show `main~1` / `HEAD` / `HEAD^` but `resolve_commit`
  rejects ancestry refspecs (tracked `acetone-bvq`) — align docs with behaviour
  for 0.1, fix refspecs in Phase 7.
- Spec §7 lists `push`/`pull`/`clone` that do not exist; spec §5.3 describes a
  Volcano/iterator executor but the engine materialises a clause pipeline;
  ADR-0017 stale. Correct the spec to match what 0.1 actually is.
- **SHA-1 default**: documented as the deliberate 0.1 choice (GitHub cannot host
  SHA-256 repos); hash width is validated-not-assumed on read.
- **Positioning**: state plainly that 0.1 is *a solo, git-native workbench for a
  version-controlled asset registry — imports become audited commits, diffs
  become change reports, any git remote is backup/transport*; the **library is
  0.2**, gated on the resource governor.

---

## 3. Post-0.1 — Phase 7 ("extend the storage discipline upward")

Phase 7 is reframed per the architecture review: *carry the lower system's
invariant discipline up into the query/runtime layer* — and, per fitness,
dogfood-UX-first. It is the last chance to fix the internal seams before any
external API commits to them.

**Runtime / query-layer maturation (the "upper system"):**

- **Resource governor** — cap execution time, result rows, expansion steps, list
  size. *The blocker for any library/embedded use.* `acetone-iq6`.
- **Lazy, store-backed `IndexSeek`** — stop materialising the whole version per
  statement; touch only matching rows. `acetone-cbl.11`, `acetone-6g5.3.3`.
- **Stable relationship identity** — replace positional `e{index}` rel ids with
  a real edge identity, *before* any API freezes it. (Root cause shared with
  U11.) New: `acetone-rid`.
- **Value-domain round-trip contract** — reads must preserve temporal/`Bytes`
  values as themselves, not debug strings (U2 is the acute symptom; the general
  fix is a typed value channel through the adapter, not string rendering). New:
  `acetone-vdc`.
- Library-level query entry point + route CLI through the façade — `acetone-vf6`,
  `acetone-ijq` (do *after* the above, so the API doesn't freeze the defects).

**Merge lifecycle & meaning:**

- **Cell-level (per-property) merge.** The design record (Decision 4) promises
  cell-wise merge; the implementation is key-level, so two branches editing
  *different properties of the same node* conflict. This is the common
  asset-registry case (import sets `os_version`, human sets `owner`) and it
  undercuts the ratified annotation-protection plan (`acetone-6g5.11`). Greg's
  call at the demo: implement cell-wise (recommended) or amend the design.
  New: `acetone-clm`.
- Merge abort + graph-violation resolution + mid-merge recovery — `acetone-mws`.
- Post-merge validation semantics + conflict model (ADR-0016) — `acetone-jmp`.
- Conflict visibility: `acetone.conflicts()` base/ours/theirs side by side —
  `acetone-s7d`.

**Dogfood UX (fitness):**

- Workspace `diff` / `discard` (see and revert uncommitted changes).
- Property-level structured (JSON) diff for change reports.
- Ancestry refspecs (`main~1`, `HEAD^`) in `resolve_commit` — `acetone-bvq`.
- Import vs human curation via import-to-branch + merge — `acetone-6g5.11`
  (depends on cell-level merge landing).
- Export hardening: relational projection to a lower crate, self-describing
  edges, faithful CSV — `acetone-6g5.9`.

---

## 4. Post-0.1 — Phase 8 ("alongside code" / co-tenancy)

Making an acetone graph a co-tenant of an ordinary git repo (its own branch,
Dolt-style, or embedded beside code). **No on-disk format obstacle** — but four
behavioural assumptions must flip *together*, best unified behind one
`GraphRefNamespace` concept:

1. `HEAD` currently means "the graph checkout"; must become "the graph's ref,
   wherever it lives".
2. `refs/heads/*` used unqualified; must be namespaced so graph refs and code
   refs coexist.
3. `migrate` rewrites *all* refs; must rewrite only the graph's refs.
4. `gc` walks *all* refs for reachability; must scope to the graph's refs (and
   never delete objects reachable only from code history).

Tracked substrate: `acetone-5w6` (co-tenancy constraint), `acetone-ejj`
(migrate ref-scoping), `acetone-7tf`/`acetone-cm9`/`acetone-060` (worktree/HEAD
edge cases). New umbrella: `acetone-gns` (`GraphRefNamespace`).

**Format-evolution policy must be settled before the first bump** (`acetone-5yr`
read-old-write-new vs. rewrite-migrate): history-rewriting `migrate` is
incompatible with the sharing pitch (it force-pushes all history and changes
every commit hash). An ADR should choose read-old-write-new as the default
evolution path and reserve rewrite-migrate for opt-in. New: `acetone-fev`
(ADR).

---

## 5. Post-0.1 — Phase 9 ("at scale & in conformance")

- Streaming / bounded-memory import for large sources — `acetone-6g5.7`.
- fsck scale: dedup chunk sets across versions, walk tags, worktree-aware —
  `acetone-7fe`, `acetone-8t3`, `acetone-6g5.10`.
- `merge_base` LCA bound (worst-case ~cubic) — `acetone-vgt`.
- openCypher conformance backlog surfaced by TCK: pattern comprehension
  (`acetone-cxh`), label predicate in expression position (`acetone-6gy`,
  `acetone-q9m`, `acetone-i8z`), `i64::MIN` literal (`acetone-4lh`), binder
  refinements (`acetone-1qj`).
- Composite index seek acceleration — `acetone-0c7`.
- Uniqueness / partial indexes — `acetone-ryg` (definition already frozen wide
  enough per ADR-0027).

---

## 6. Tooling / extension opportunities (beyond)

Captured from the fitness and architecture reviews as future value, not 0.1
scope:

- **`acetone log` / `acetone blame`** over graph history (blame substrate exists:
  `acetone-596`).
- **Watch/reactive queries** and **materialised views** built on the derived-map
  machinery (indexes are the first instance).
- **Structured change-report export** (diff → PR-style artefact) as the natural
  product of the diff engine.
- **Schema evolution tooling** riding on `migrate` (rename label/property with
  history rewrite or read-old-write-new).
- **Embedded/library** use once the governor lands (0.2): the `GraphSource`
  trait and `acetone-core` façade are the seams to stabilise then — deliberately
  *not* stabilised now, so the U2/U11/rel-identity fixes are free to change them.

---

## 7. What is explicitly *not* changing for 0.1

- The **on-disk format** (`format_version = 1`, golden-pinned). Gate D holds;
  composite index keys already folded in (ADR-0027) so no future migration.
- The **invariant core** (prolly + model). Reviewed clean.
- **Crates.io publication** — not happening at 0.1 by decision (Greg,
  2026-07-10). The crates stay clean and buildable but internal; no external API
  is frozen, which is exactly why the Phase-7 seam fixes (U2, U11, rel identity,
  query API) are cheap.
- The **merge granularity decision** is deferred to the demo (§3, `acetone-clm`),
  not silently shipped as key-level.

---

## 8. Sources

Four fresh Fable-5 adversarial reviewers, no implementation context:
architecture (18 findings, verdict "two systems of different maturity"), code
(25 findings, 10 urgent), security (13 findings, verdict "the CLI is responsible
to distribute via Homebrew after the pre-tag fixes; the library must wait for
the governor"), fitness/docs (22 findings + the 0.1 positioning). Findings were
deduplicated across dimensions (e.g. merge×index appears in both code and
architecture; the value-domain corruption in architecture is the acute form of
the fitness "temporal renders as debug string" note).
