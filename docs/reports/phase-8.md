# Phase 8 report — alongside code

*Epic `acetone-g5g` · target 0.3 · base `main @ 959d1ab` (v0.2.0) · this report at branch tip including `acetone-5yr`*

Phase 8 makes an acetone graph a **co-tenant of an ordinary git repository**:
its own ref, living alongside code history in one object store, with code
branches and graph branches coexisting untouched. There was no on-disk format
obstacle to this — the obstacle was four behavioural assumptions baked into the
ref/store layer, all of which had to flip *together*. This phase flips them
behind one concept (`GraphRefNamespace`), proves that the destructive operations
(`gc`, `migrate`) stay in the graph's lane, and ships the machinery for evolving
the format *without rewriting shared history*.

Everything landed autonomously under the usual gate: per bead a design recorded
in the bead, TDD, a fresh strongest-tier (Opus) adversarial review with no
implementation context, fix/re-review, squash-merge on green CI, close.

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

## Gate evidence — 0.3 exit criteria

**Exit criterion 1 — a graph on its own ref inside a code repo, branches coexisting untouched.**
`GraphRefNamespace::co_tenant(graph)` (ADR-0049/0050) puts graph branches under
`refs/heads/acetone/<graph>/*` (a proxy-safe subnamespace of `refs/heads`), tags
under `refs/tags/acetone/<graph>/*`, and the graph's current-branch pointer at a
local-only symref `refs/acetone/<graph>/HEAD` — so the user's git `HEAD` and code
branches are never touched. `acetone-mgf` (#147) adds the init opt-in, the marker,
and open-time detection that selects this layout. **Met.**

**Exit criterion 2 — `migrate` and `gc` provably touch only graph refs.**
`acetone-iva` (#148) proves it with property tests: an object reachable only from
code history survives `gc`, and code refs survive `migrate`. The `gc` half is met
under the **interpretation recorded in ADR-0051 (reading A)**: acetone `gc`
repacks the whole repository's reachable set (code included, exactly as `git gc`
would) but moves no code ref and changes no commit hash — the co-tenancy promise
*"the graph never rewrites or moves your code history"* holds. **Met — with an
interpretation for Greg to rule on (see Decisions).**

**Exit criterion 3 — a `format_version` bump applied live via read-old-write-new, no rewrite, no force-push.**
`acetone-5yr` (#149) ships the dispatch machinery (ADR-0052). Because the manifest
envelope is the stable `[format_version, body]`, `decode` reads the version first
and dispatches to a retained per-version decoder; a repository may hold commits at
several versions side by side, old ones readable, none rewritten. Proven by a
coexistence test: a content-addressed store holds a v1 and a (synthetic) v2
manifest together, both decode, the v1 object's hash is unchanged by the v2 write,
and re-encoding still emits current-format bytes at the same address. **Met in the
"machinery shipped + cross-version coexistence proven" sense — a deliberate
deviation (option B, ADR-0052) from shipping a real `format_version = 2`, for
Greg to rule on (see Decisions).**

## Decisions taken — ADRs 0048–0052 (the ratification agenda)

Four (0048, 0049, 0050, 0052) are `accepted — pending ratification at the Phase 8
boundary`; **ADR-0051 is `proposed — flagged for Greg's ruling`** (an
exit-criterion interpretation, not yet accepted). Two carry an explicit **ruling**
for Greg beyond plain ratification.

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

- **ADR-0051 — Co-tenant `gc` semantics — ⚑ RULING.**
  Ships **reading (A)**: `acetone gc` repacks the whole repository's reachable set
  (code + graph), like `git gc` — byte-preserving, `fsck`-clean, no ref moved, no
  hash changed. This is the recorded interpretation of exit criterion 2's `gc`
  half. **Reading (B)** — scope `gc`'s reachable set/pack to the graph's refs while
  still treating code refs as un-prunable roots — is the deferred, stronger
  alternative (more intricate reachability split; no format impact either way).
  **Greg rules: accept (A) as shipped, or direct (B).**

- **ADR-0052 — `format_version` dispatch machinery; synthetic-v2 proof; defer a real v2 — ⚑ RULING.**
  Ships the dispatch machinery and proves it with a *test-only* synthetic
  `format_version = 2`, keeping `FORMAT_VERSION = 1` — **zero format change, zero
  golden churn** (option B). A real shipped v2 is deferred to the first genuine
  format change, which will land through this same seam (the ADR-0025 "engine now,
  real demonstration deferred" precedent, which Greg accepted at 0.2). This is a
  deliberate deviation from ADR-0048's note anticipating a real v2 in Phase 8, so
  **exit criterion 3 is met in the "machinery + coexistence proven" sense**.
  **Greg rules: accept that reading, or require a real `format_version = 2` before
  the gate closes.**

## Review findings summary

Every code PR passed a fresh Opus adversarial review with no implementation
context; the fresh reviewer caught a real defect on the two exit-criterion PRs:

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
marker but this yields no escape. The ADR-0051 gc reading (A) — `acetone gc`
repacking code objects — is a deliberate exit-criterion interpretation for Greg,
not a defect.

Pre-existing, rediscovered and out of Phase 8 scope: the `write_ref` CAS TOCTOU vs
a non-acetone writer (acknowledged in-code) and `gc`'s lock-free reachability
(`acetone-dfh`, P3). Neither is introduced or worsened by this phase.

## Open risks and deferred work

- **`acetone-ejj` — migrate hardening (P3, open).** Rewrite annotated-tag objects
  and make the ref swings a single atomic transaction. `migrate` already
  namespace-scopes via the `gns` seam; this is robustness, not a correctness gap
  for the exit criteria.
- **`acetone-xg6` — CLI `--co-tenant` flag (P3).** Co-tenant init is wired at the
  library layer; exposing it on the CLI is a follow-up.
- **`acetone-eo7` — co-tenant init hardening (P3).** Legacy-workspace guard +
  edge-case tests.
- **`acetone-dfh` — gc `has_linked_worktrees` TOCTOU (P3, pre-existing).** Relevant
  to co-tenant `gc` scoping; lock-free check races a concurrent `git worktree add`.
- **A real `format_version = 2`** is deferred to the first genuine format change
  (ADR-0052), landing through the shipped dispatch seam.
- **Spec §10 wording (minor).** §10 still frames `migrate` as *the* evolution
  mechanism; it should gain a read-old-write-new/dispatch sentence when Greg
  ratifies ADR-0048 (a governing-doc edit, deliberately deferred to ratification).

**Gate readiness: the milestone security review returned READY (no blocker/high),
so on security grounds the gate is ready to close.** The two exit-criterion
interpretations (ADR-0051 gc reading, ADR-0052 exit-criterion-3 reading) are
Greg's to rule on at the boundary; the gate (`acetone-g5g`) is his to close.

## The demo

The live demo drives the phase's actual code step by step (one step per turn):
create a code repo, init an acetone graph as a co-tenant inside it, show code
branches and graph refs coexisting (`refs/heads/acetone/…` vs the user's
`refs/heads/*`), commit graph data, run `gc` and show code objects preserved,
and walk the `format_version` dispatch that lets old and new commits coexist.
