# Phase 8 report — alongside code

*Epic `acetone-g5g` · target 0.3 · base `main @ 959d1ab` (v0.2.0) · this report covers the phase through `acetone-xg6` (PR #156)*

Phase 8 makes an acetone graph a **co-tenant of an ordinary git repository**:
its own ref, living alongside code history in one object store, with code
branches and graph branches coexisting untouched. There was no on-disk format
obstacle to this — the obstacle was four behavioural assumptions baked into the
ref/store layer, all of which had to flip *together*. This phase flips them
behind one concept (`GraphRefNamespace`), makes the destructive operations
(`gc`, `migrate`) stay in the graph's lane — `gc` **graph-scoped** so it never
disturbs the user's code storage, and `.keep`-protected so a foreign `git gc`
cannot degrade it — and ships the machinery for evolving the format *without
rewriting shared history*.

Everything landed autonomously under the usual gate: per bead a design recorded
in the bead, TDD, a fresh strongest-tier (Opus) adversarial review with no
implementation context, fix/re-review, squash-merge on green CI, close. At the
boundary Greg ratified ADRs 0048/0049/0050, ratified ADR-0052 (option B), and
ruled ADR-0051 to reading **(B) graph-scoped** — which was then *delivered with
full assurance* (`acetone-wao` + `acetone-5cw`, ADR-0053) rather than shipped as
the reading-(A) interim, so exit criterion 2 is met under the adopted semantics.

## What shipped

| Bead | PR | What |
|------|----|------|
| `acetone-fev` | #143 | **ADR-0048** — read-old-write-new is the default format-evolution path; history-rewriting `migrate` is retained but opt-in. The Phase 8 forward gate, decided by ADR so work could proceed. |
| `acetone-gns` | #144 | **`GraphRefNamespace`** (ADR-0049) — one value that maps a graph's logical refs (branches, tags, head pointer) to physical git ref paths. A behaviour-preserving seam: ships the type + the standalone layout, routes every ref-path site through it. Byte-identical; goldens unchanged. |
| `acetone-060` | #145 | **Detached-HEAD merge precondition** — `merge()` reports `NoCurrentBranch` before `DirtyWorkspace` on a detached HEAD, so the diagnostic matches the actual precondition failure. |
| `acetone-0e4` | #146 | **Named head pointer** (ADR-0050) — generalise the store's head plumbing from git `HEAD` only to a *named* pointer ref, and add the co-tenant layout to `GraphRefNamespace`. Standalone keeps the git-`HEAD` fast path and stays byte-identical. |
| `acetone-mgf` | #147 | **Co-tenant mode selection** (ADR-0050) — `init` opt-in + an on-disk marker + open-time detection, wiring the `co_tenant` namespace. **Exit criterion 1.** Review caught a code-loss crash window between marker write and first commit → fixed to marker-first ordering. |
| `acetone-iva` | #148 | **gc/migrate ref-scoping proof** (ADR-0051) — property tests that a code-only object survives `gc` and code refs survive `migrate`. **Exit criterion 2.** Review caught a vacuous gc test → strengthened with a loose-file discriminator that only passes if `gc`'s reachable set genuinely includes code. |
| `acetone-5yr` | #149 | **`format_version` dispatch machinery** (ADR-0052) — `Manifest::decode` dispatches on the manifest's version to a table of retained per-version decoders, instead of rejecting any non-current version. **Exit criterion 3.** Read-old, write-new: old commits stay readable through their era's decoder; new writes emit the current format; nothing is rewritten. |
| `acetone-wao` | #152 | **Graph-scoped `gc`** (ADR-0051 reading B) — `consolidate_scoped` packs only objects reachable from the graph's refs, with a prune guard so no object reachable from a non-graph ref is ever disturbed; `GraphRefNamespace::owns_ref` classifies. **Delivers exit criterion 2's gc-half under (B)**, replacing the (A) interim; standalone byte-identical. Review caught a blocker — `owns_ref` misclassified `refs/remotes/*` (a clone's code) as the graph's → fixed layout-aware, with a remotes discriminator test. |
| `acetone-5cw` | #153 | **`.keep`-marked packs** (ADR-0053) — acetone marks its consolidation pack `.keep` so a foreign `git gc`/`git repack` (incl. `gc.auto`) leaves its content-aware deltas intact. Only possible cleanly *because* the pack is now graph-only (B). Proven against the real `git repack -a -d`. |
| `acetone-xg6` + `acetone-eo7` | #156 | **`acetone init --co-tenant <graph>` on the CLI** — makes co-tenant init reachable from the shipped tool (it was library-only), wiring to `Repository::init_co_tenant`; plus the `eo7` legacy-workspace guard (reject a pre-ADR-0014 standalone workspace) and CLI edge-case tests. Pulled into the phase at the boundary — see the process note below. |

## Gate evidence — 0.3 exit criteria

**Exit criterion 1 — a graph on its own ref inside a code repo, branches coexisting untouched.**
`GraphRefNamespace::co_tenant(graph)` (ADR-0049/0050) puts graph branches under
`refs/heads/acetone/<graph>/*` (a proxy-safe subnamespace of `refs/heads`), tags
under `refs/tags/acetone/<graph>/*`, and the graph's current-branch pointer at a
local-only symref `refs/acetone/<graph>/HEAD` — so the user's git `HEAD` and code
branches are never touched. `acetone-mgf` (#147) adds the init opt-in, the marker,
and open-time detection that selects this layout. **Met.**

**Exit criterion 2 — `migrate` and `gc` provably touch only graph refs.**
`migrate` is graph-scoped via the `GraphRefNamespace` seam (`acetone-iva` proves
code refs and git `HEAD` survive it). For `gc`, Greg **ruled reading (B)** at the
boundary and directed it be *delivered with full assurance*: `acetone-wao` (#152)
makes `consolidate` pack only objects reachable from the graph's refs and adds an
explicit prune guard over non-graph refs, so acetone leaves the user's code
objects' storage exactly as git had it — it neither repacks nor prunes them. The
proof was rewritten to the discriminating (B) property: after `gc` a code-only
object is **still loose and not in any acetone pack**, yet retrievable, while the
graph's own loose objects are consolidated away (a property reading A could not
satisfy). A second test covers the realistic clone shape — a code object
reachable only from `refs/remotes/*` is guarded identically. Standalone
consolidation is byte-identical (its graph refs are all refs, so the guard is
empty). **Met under the adopted reading (B).**

**Exit criterion 3 — a `format_version` bump applied live via read-old-write-new, no rewrite, no force-push.**
`acetone-5yr` (#149) ships the dispatch machinery (ADR-0052). Because the manifest
envelope is the stable `[format_version, body]`, `decode` reads the version first
and dispatches to a retained per-version decoder; a repository may hold commits at
several versions side by side, old ones readable, none rewritten. Proven by a
coexistence test: a content-addressed store holds a v1 and a (synthetic) v2
manifest together, both decode, the v1 object's hash is unchanged by the v2 write,
and re-encoding still emits current-format bytes at the same address. **Met in the
"machinery shipped + cross-version coexistence proven" sense — Greg ratified this
reading (ADR-0052, option B) at the boundary, deferring a real `format_version = 2`
to the first genuine format change.**

## Decisions — ADRs 0048–0053 (ratified / ruled at the boundary)

Greg's boundary rulings (2026-07-22), now recorded in each ADR's status line:
0048, 0049, 0050 **ratified**; 0052 **ratified (option B)**; 0051 **ruled (B)**
and delivered; ADR-0053 (`.keep` durability) taken in the course of delivering B.

- **ADR-0048 — Format evolution: read-old-write-new is the default; rewrite-`migrate` is opt-in.**
  Rewriting history to cross a format boundary changes every commit hash and needs
  a force-push — fine for a standalone repo that *is* the graph, unacceptable for
  a graph sharing a repo (and collaborators) with code. So the default is
  read-old-write-new (retain old decoders, never rewrite); `migrate` stays as the
  deliberate opt-in for standalone repos that want a single-format history. *Ratify.*

- **ADR-0049 — `GraphRefNamespace`: centralise a graph's ref-path mapping.**
  One value maps logical refs → physical ref paths; a `Repository` holds one,
  built at `init`/`open`. Resolves the standalone-vs-co-tenant fork as *one
  parameterised code path, two layouts*, not two divergent paths. `gns` ships the
  seam + the standalone layout, byte-identical. *Ratify.*

- **ADR-0050 — Co-tenant mode: layout + named head pointer.**
  The co-tenant layout (`refs/heads/acetone/<graph>/*`, `refs/tags/acetone/<graph>/*`,
  local-only `refs/acetone/<graph>/HEAD`) plus a store head-pointer generalisation
  so *the layout, not the code path*, decides where a graph's refs and
  current-branch pointer live. Standalone keeps the git-`HEAD` fast path and is
  byte-identical. *Ratify.*

- **ADR-0051 — Co-tenant `gc` semantics — RULED (B), delivered.**
  Greg ruled reading **(B) graph-scoped** — `acetone gc` packs only the graph's
  objects and leaves the user's code storage untouched — and chose to *deliver it
  with full assurance* rather than ship the reading-(A) interim. Delivered in
  `acetone-wao` (#152): graph-scoped consolidation with a non-graph prune guard,
  the proof rewritten to (B), standalone byte-identical. The earlier (A) —
  repo-global repack — is retired. This is the reading that lets ADR-0053 protect
  acetone's pack without freezing the user's code-object packing.

- **ADR-0052 — `format_version` dispatch machinery; synthetic-v2 proof; defer a real v2 — RATIFIED (option B).**
  Ships the dispatch machinery and proves it with a *test-only* synthetic
  `format_version = 2`, keeping `FORMAT_VERSION = 1` — **zero format change, zero
  golden churn**. A real shipped v2 is deferred to the first genuine format change,
  which will land through this same seam (the ADR-0025 "engine now, real
  demonstration deferred" precedent). Greg ratified this reading of exit criterion
  3 at the boundary.

- **ADR-0053 — `.keep`-marked consolidation packs — accepted (in delivering B).**
  A foreign `git gc`/`git repack` (including git's automatic `gc.auto`, which a
  co-tenant repo's owner triggers routinely) would re-deltify acetone's pack and
  undo its content-aware deltas — safe-but-lossy, silently. acetone now marks its
  pack `.keep` so git leaves it alone; `supersede_packs` retires the marker with
  the pack. Cleanly possible only because reading (B) makes the pack graph-only.
  Proven against the real `git repack -a -d`.

## Review findings summary

Every code PR passed a fresh Opus adversarial review with no implementation
context; the fresh reviewer caught a real defect on most of the substantive PRs
— the gate is load-bearing:

- **`acetone-mgf` (#147):** a code-loss crash window between writing the co-tenant
  marker and the first commit — fixed to marker-first ordering so a crash leaves a
  recoverable, not a corrupt, state.
- **`acetone-iva` (#148):** the initial `gc` proof test was vacuous (it would pass
  even if `gc` scoped incorrectly) — strengthened with a loose-file discriminator
  that only passes if `gc`'s reachable set genuinely draws in the code object.
- **`acetone-5yr` (#149):** APPROVE — no correctness/totality/freeze defect. The
  reviewer confirmed the v1 decode path is byte-identical to before, decoders stay
  panic-free and total (the `u32::try_from` guard blocks a `2^32+k`
  truncation-mis-dispatch), the format freeze is intact (`FORMAT_VERSION` still 1,
  no golden churn), and the coexistence test is honest — its `blob_hash` matches
  the address a real `GitStore` assigns, not a mock. One minor (spec §10 wording,
  below), one informational (the exit-criterion-3 deviation is ADR-owned and
  flagged, not a quiet under-delivery).
- **`acetone-wao` (#152):** review found a **blocker** — `owns_ref`'s fallthrough
  claimed `refs/remotes/*`, `refs/notes/*`, `refs/stash`, `refs/replace/*` as the
  graph's, so a graph in a *cloned* code repo (the normal case) would draw its
  remote-tracking code objects into acetone's pack. Fixed layout-aware
  (`owns_whole_repo` flag: standalone owns all, co-tenant owns only its prefixes +
  `refs/acetone/*`), with a new discriminator test verified to fail under the bug.
- **`acetone-5cw` (#153):** APPROVE — the reviewer independently reproduced the git
  `.keep` behaviour and confirmed no object-loss path and that the durability test
  truly discriminates. One optional wording nit on `ensure_keep`'s failure
  semantics, addressed by a doc clarification.

## Milestone security review

A fresh strongest-tier (Opus) security review swept the whole phase diff
(`959d1ab..HEAD`), focused on the surfaces Phase 8 opened: untrusted-manifest
handling through the new version dispatch, ref/path injection across the co-tenant
namespace, and the scoping of the destructive operations.

**GATE VERDICT: READY** — no blocker or high-severity findings. Verified:

- **`format_version` dispatch is total and truncation-safe.** Version read as
  `u64`, then `u32::try_from(...).ok()` before a bounded table lookup, so `2^32+k`
  cannot alias version `k`; unknown/future versions fail loudly with
  `UnsupportedVersion`; the index-map `count > remaining` DoS guard is intact; no
  panics introduced; the v1 body reader is byte-identical.
- **Ref/path injection is contained.** Every ref write flows through
  `validated_ref_name` (`refs/` prefix + `gix FullName::try_from`, rejecting `..`,
  control chars, malformed components). `co_tenant(graph)` only ever *suffixes* a
  validated prefix, and `validate_graph_name` rejects `/`, `..`, leading `.`,
  `.lock`, control/space/`~^:?*[\` at `init_co_tenant`.
- **A forged co-tenant head symref cannot write to code.** `current_branch()`
  filters the symref target through `namespace.branch_name(...)`, so a target
  pointing at `refs/heads/main` yields `NoCurrentBranch`; `commit` and `merge` take
  the branch from `current_branch()`, so no code ref can be advanced. No infinite
  recursion on a symbolic branch target.
- **Destructive-op scoping.** `migrate::rewrite_history` sources its ref set from
  `namespace.branch_prefix()`/`tag_prefix()` — graph-scoped in co-tenant mode; the
  `gc` proof test is non-vacuous (a code blob is consolidated yet still
  retrievable, which only holds if code is a reachability root).
- **Crash safety.** Marker-first `init_co_tenant` ordering fails safe (an
  interrupted init opens as co-tenant → `NoWorkspace`, never as standalone writing
  onto `refs/heads/main`).
- **Reduced-trust posture intact.** The isolated-open committer identity is
  hardcoded literal strings (not attacker-controlled, plain value keys). No new
  dependencies (no `Cargo.toml`/`Cargo.lock` change).

Low/informational (no gate action): co-tenant marker refs are local-only, so the
hostile-marker scenarios are only reachable via a hand-delivered on-disk `.git`,
not a network clone — and even then all writes stay namespace-scoped and
validated; `detect_namespace` does not re-validate a name read from an existing
marker but this yields no escape.

Pre-existing, rediscovered and out of Phase 8 scope: the `write_ref` CAS TOCTOU vs
a non-acetone writer (acknowledged in-code) and `gc`'s lock-free reachability
(`acetone-dfh`, P3). Neither is introduced or worsened by this phase.

**Security re-touch over the reworked `gc` (`acetone-wao` + `acetone-5cw`).** The
first sweep predated the reading-(B) rework, so a focused fresh-Opus security pass
ran over the new consolidation internals (`d1d8b8c..HEAD`): the graph-scoped
reachability + prune guard, `owns_ref` as a security boundary (can a crafted ref
name get a code ref repacked/pruned, or escape the graph namespace?), `.keep`
path handling, and panic/totality on a hostile repo. **Verdict: READY** — no
blocker/high. The central finding: `gc` can never delete the last copy of any
object *regardless of whether `owns_ref` is correct*, because pruning is gated on
membership in a freshly `fsync`ed pack behind a set-equality tripwire and the
prune guard is strictly subtractive — `owns_ref` governs only ownership/efficiency
(what gets repacked vs left as git arranged it). Ref-resolution failures, symref
cycles, missing targets and dangling refs are all skipped without panic; both
walks are iterative and size-capped. Three LOW/latent notes, none blocking, filed
as `acetone-c2a`: a multi-graph `refs/acetone/` over-claim the current
single-graph restriction makes unreachable; an unvalidated marker-derived graph
name (bounded to deletion-safe misclassification); and a pre-existing
sidecar-stem path use reachable only via direct `.git` tampering.

## Open risks and deferred work

- **`acetone-ejj` — migrate hardening (P3, open).** Rewrite annotated-tag objects
  and make the ref swings a single atomic transaction. `migrate` already
  namespace-scopes via the `gns` seam; this is robustness, not a correctness gap
  for the exit criteria.
- **`acetone-dfh` — gc `has_linked_worktrees` TOCTOU (P3, pre-existing).** Relevant
  to co-tenant `gc` scoping; lock-free check races a concurrent `git worktree add`.
- **A real `format_version = 2`** is deferred to the first genuine format change
  (ADR-0052), landing through the shipped dispatch seam.
- **A foreign `git repack` may leave duplicate storage** for a window (git packs
  some graph objects into its own pack before acetone next consolidates) — harmless
  duplication the next `acetone gc` resolves (ADR-0053).

Spec §10 was updated at ADR-0048's ratification to record the read-old-write-new
default (PR #151), closing the earlier wording gap.

## Process note — a feature isn't delivered until it's reachable

Co-tenant init shipped as `Repository::init_co_tenant` (`acetone-mgf`) with the CLI
surface split into a separate P3 bead (`acetone-xg6`) and deferred past the
boundary. Because the exit criteria were framed around the **mechanism** (a graph
on its own ref; `gc`/`migrate` scoped; a format bump via read-old-write-new),
every criterion went green at the library layer while the feature stayed
unreachable from the shipped tool — a user could not create a co-tenant graph
without writing Rust. Surfaced at the boundary (Greg: *"how was it ever deferred?
it is effectively deferring the feature"*), `xg6` (+ `eo7`) was pulled into the
phase and delivered under the full gate before closing.

The lesson, recorded so it doesn't recur: **a user-facing feature is not delivered
until it is reachable through the shipped interface**, and exit criteria / bead
decomposition should say so — "the mechanism works" must not be allowed to stand
in for "the feature is usable". (Whether to codify this as a working agreement in
`CLAUDE.md` is Greg's call — flagged here, not changed unilaterally.)

**Gate readiness.** All three exit criteria are met — criterion 2 under the
adopted reading (B), delivered with full assurance — and co-tenancy is now usable
end-to-end through the `acetone` CLI (`init --co-tenant …` through `gc`). Greg has
ratified ADRs 0048/0049/0050/0052/0053 and ruled 0051 (B). The milestone security
review (and the re-touch over the reworked `gc`) returned READY. What remains is
Greg's to do: **close the exit-criteria gate (`acetone-g5g`)** — which agents
never close.

## The demo

The live demo drives the phase's actual code step by step (one step per turn):
create a code repo, init an acetone graph as a co-tenant inside it, show code
branches and graph refs coexisting (`refs/heads/acetone/…` vs the user's
`refs/heads/*`), commit graph data, run `gc` and show code objects preserved,
and walk the `format_version` dispatch that lets old and new commits coexist.
