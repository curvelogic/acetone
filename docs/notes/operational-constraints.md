# Operational constraints

Environment facts that shape design and workflow decisions. Add entries as
they are discovered; each states its consequence, not just the observation.

## Git proxies may refuse non-standard ref namespaces (2026-07-04)

Observed by Greg in Claude Code web environments: the git proxy passes
`refs/heads/*` and `refs/tags/*` but returns 403 for `refs/dolt/*`.
Infrastructure (proxies, hosting ACLs, CI mirrors) can *actively refuse*
custom namespaces — a stronger constraint than "default refspecs don't
fetch them".

**Consequences:**

- Acetone design rule (recorded on bead acetone-63m.5, to be reflected in
  the spec §3.5 revision): anything that must survive transfer lives in
  `refs/heads/*` or `refs/tags/*` only. Custom namespaces
  (`refs/acetone/workspaces/*`) may hold only state that is correct to lose
  on clone and never needs to cross a proxy. Workspace/WORKING state
  qualifies; conflict state and import bookmarks must not rely on
  custom-namespace sync.
- Beads sync (`refs/dolt/data`) fails in such environments — treat `bd`
  Dolt sync as desktop-only for this repository.
