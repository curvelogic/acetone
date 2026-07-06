# ADR-0015: Anchor workspace chunk sets against foreign gc

*Status: accepted (agent decision, flagged for phase-boundary review) · Date: 2026-07-06 · Bead: acetone-huo · Amends: ADR-0010, ADR-0014*

## Context

ADR-0010 pointed the workspace ref straight at the manifest *blob*. Git
keeps the blob reachable, but it cannot parse a manifest, so the prolly
chunks the manifest references are **not** reachable from the blob ref. An
uncommitted workspace therefore did not survive an aggressive foreign
`git gc --prune=now` — the documented "commit before any external gc"
caveat. Phase 3's interactive write path (acetone-mex.2) builds `save` on
the workspace, so uncommitted state becomes routine and the caveat becomes
a real durability hazard. Greg steered this to be settled before the write
path lands.

## Decision

**The workspace ref points at a workspace tree, not a bare blob.** The ref
target is a git tree `{manifest: <blob>, chunks/: <anchor tree>}` — exactly
the shape `create_commit` already builds for a commit, minus the `README.md`
summary. The `chunks/` anchor tree is the same sharded `<hh>/<rest-of-hex>`
tree of chunk-blob references the commit path uses (`write_anchor_tree`).
Git's reachability walk follows the tree into `chunks/` and keeps every
referenced chunk, so an uncommitted workspace survives any gc — foreign or
acetone's own. This closes the ADR-0010 caveat.

**It is a local-only ref-plumbing change, not a format change.** The
`Manifest` bytes and every chunk are byte-identical; only what the ref
*points at* changes (blob → tree). Workspace refs are never pushed
(ADR-0010), so no history migration and no `format_version` bump. The
anchor tree references existing blobs, so it costs no chunk storage, and
unchanged shard trees dedupe across saves and commits.

**Reads resolve blob-or-tree transparently.** `GitStore::workspace_manifest_hash`
peeks the ref target's object kind: a tree yields its `manifest` entry
blob; a bare blob (a workspace last written before huo, or via the ADR-0014
legacy fallback) is the manifest directly. So the graph layer reads any
workspace uniformly across the migration; a pre-huo workspace is rewritten
as a tree on its next content-changing `save`/`checkout`.

**Anchoring is recomputed per save (naive), for now.** Each `save` builds
the workspace tree from the manifest's *complete* reachable chunk set
(`manifest_chunk_set`, the same routine `commit` uses). This is correct and
simple; the cost is O(total chunks) per save, which the anchor tree's shard
dedup mitigates in *storage* but not in *walk time*. The incremental path
Greg asked for — maintain the anchor tree in ~O(changed chunks) using
pack-on-write's added/orphaned-chunk delta (ADR-0011, acetone-63m.13) — is
deferred to a tracked optimization (acetone-taf) so the write
path builds on a *correct* anchored workspace now. This is the one
deviation from the bead's "build the incremental path" steer; it is a
latency optimization over a correct base, flagged for the phase report.

## Consequences

- Uncommitted working state is fully gc-durable; the "commit before gc"
  caveat is dropped from the docs. Verified by a test that saves (never
  commits) a multi-chunk workspace, runs `git gc --prune=now --aggressive`,
  and reads it back cold in full.
- fsck resolves the workspace ref (tree or blob) to its manifest and checks
  it; anchor-completeness of the workspace tree itself (that every anchored
  chunk exists) is a natural extension (acetone-5a8).
- `gc.auto` handling and acetone's own gc interaction with the anchored
  workspace remain as they were; huo only guarantees the chunks are
  *reachable*, which is what protects them.
- Per-save anchoring latency on large graphs with frequent interactive
  saves is the known cost until the incremental path lands.
