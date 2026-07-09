# ADR-0023: Namespace machine-readable tree entries under `.acetone/`

*Status: proposed · Date: 2026-07-09 · Bead: acetone-gbd*

## Context

An acetone commit is a git commit object whose tree carries acetone's
machine-readable state (spec §3.5). Until now that tree placed three entries at
its **root**:

- `manifest` — the blob the model layer decodes into a graph version;
- `chunks/` — a sharded anchor tree that keeps every referenced chunk
  reachable under `git gc`/`clone`/`push` (ADR-0010, amended by acetone-huo);
- `README.md` — a small human-readable summary so a hosting UI (GitHub, …)
  auto-renders something meaningful when a repository is browsed.

The workspace tree (the Dolt-style working state under
`refs/acetone/workspaces/*`, whose `{manifest, chunks/}` shape ADR-0015
established to survive foreign `git gc`) placed `manifest` and `chunks/` at its
root too; this ADR amends that layout.

These names are acetone conventions, not git requirements. The moment acetone
shares a repository or a tree with other content (the shared-tree co-tenancy
direction; the embedded separate-branch model acetone-5w6), squatting top-level
names is a collision hazard: `README.md` is the sharpest (an extremely common
user file), and `manifest`/`chunks` are plausible too.

Deciding this **now** is cheap: with ≈no data in the wild, changing the tree
envelope is a code change. Deciding it **after Gate D (format freeze,
acetone-cbl.1)** makes it a commit-OID-altering change needing `acetone
migrate`. This bead exists so the format ships already-namespaced and Gate D
never has to migrate it.

## Decision

**Namespace acetone's machine-readable entries under a reserved `.acetone/`
directory; keep `README.md` at the tree root.** (Option (a) of the bead's
three.)

Tree shapes (git orders trees by name, treating a directory as if it had a
trailing `/`; `gix` does **not** sort on write, so the builder orders entries
explicitly):

| Tree | Root entries | `.acetone/` subtree |
|------|--------------|---------------------|
| Commit | `.acetone/` (tree), `README.md` (blob) — `'.'` (0x2E) < `'R'` (0x52) | `chunks/` (tree, iff anchors), `manifest` (blob) — `chunks` < `manifest` |
| Workspace | `.acetone/` (tree) | same as commit |

A workspace tree's `.acetone/` subtree is byte-identical to the corresponding
commit's (same manifest blob + same anchor tree), so the two share the object.

### Why keep `README.md` at the root

The standalone auto-render is a deliberate ergonomic: browse an acetone repo on
a hosting UI and you see a human summary without knowing acetone exists.
Namespacing it under `.acetone/` would lose that for zero gain today, because
the residual collision it leaves — a user's *own* root `README.md` in a **true
shared tree** — only bites in shared-tree co-tenancy, which has a strictly
harder unsolved problem anyway: you also must not materialise thousands of
content-addressed chunk blobs into the working directory (the git-LFS/DVC
problem). Shared-tree mode is therefore out of scope here; if it lands it will
decide root-`README` placement together with the materialisation design, as its
own format consideration. The mode-dependent option (c) — standalone keeps the
root README, co-tenant hides everything — was rejected as premature: two
envelopes and a mode flag for a mode that does not exist.

### Scope and compatibility

- **Envelope only.** This changes commit- and workspace-tree OIDs. It does
  **not** touch manifest bytes, chunk hashes, or any prolly map-root hash, so
  the Load-Bearing history-independence and deterministic-encoding invariants
  are untouched. No `format_version` bump is taken here — Gate D introduces that
  machinery; this simply establishes the layout it will freeze.
- **Clean break, no back-compat reader.** With no data in the wild and the
  format unfrozen, readers require the new layout; there is no un-namespaced
  fallback for commit trees. The pre-huo bare-blob workspace-ref path is
  orthogonal and retained.
- **Reachability unaffected.** `git`'s reachability walk is transitive, so
  moving `chunks/` one level deeper keeps the huo durability guarantee. `git
  fsck --strict` still passes.
- **Generic walkers unaffected.** Consolidation (ADR-0011) walks the object
  graph by decoding every tree entry, not by name; `fsck` reads through
  `read_commit`/`workspace_manifest_hash`, both updated here.

## Consequences

- `acetone-store` gains a reserved-dir constant and two helpers
  (`write_acetone_subtree`, `root_manifest_hash`); `create_commit`,
  `write_workspace_tree`, `read_commit`, and `workspace_manifest_hash` route
  through them.
- The `manifest`/`chunks` paths a git user types move from `<ref>:manifest` /
  `<ref>:chunks/<hh>/<rest>` to `<ref>:.acetone/manifest` /
  `<ref>:.acetone/chunks/<hh>/<rest>`; `<ref>:README.md` is unchanged.
- Spec §3.5 is updated to document the `.acetone/` machine directory alongside
  the root README.
- Shared-tree co-tenancy (root-README collision + working-dir materialisation)
  remains a separate, harder question, co-designed with embedded mode
  (acetone-5w6) if and when it is scheduled.
