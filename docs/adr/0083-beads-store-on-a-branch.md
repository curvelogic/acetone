# ADR-0083: Keep the beads store on the `beads-data` branch

*Status: proposed — a governing-document change for the owner to merge ·
Date: 2026-11-17 · Bead: a `decision` bead is filed once this merges (no bd
writes were allowed between the store copy and the merge) · Tracking:
curvelogic/curve-hq#39 · Partly supersedes: ADR-0078*

## Context

ADR-0078 adopted curve-rig's inbox-branch relay because a cloud session
cannot push `refs/dolt/data`: the session's git proxy forwards only
branch creates and fast-forwards under `refs/heads/*`. The relay works,
but it fails badly when stuck. A delta refused by its staleness gate
stays on its branch and fails every later run. Acetone's two Phase 13
deltas of 3 Nov were stuck that way until the owner deleted them on
17 Nov, and three of their writes never landed.

Dolt's git remotes take a ref. `dolt remote add --ref
refs/heads/beads-data` keeps the store on an ordinary branch, which the
proxy forwards. curve-rig supports this behind a switch (rig's
`docs/relay.md` → "A store on a branch"), and snipx was the pilot
(curvelogic/snipx#37).

## Decision

Keep acetone's store on `refs/heads/beads-data`, named in
`.rig/beads-ref`:

- rig provisions a cloud session's store with `dolt clone --ref` from
  the branch, and `rig-relay stage` runs `bd sync`, which pulls, merges
  and pushes. It refuses a store whose `origin` is not the branch.
- Local clones repoint their store's `origin` to the branch (`CLAUDE.md`,
  *Branch & Merge Discipline*).
- The store was copied once, as a fast-forward of the same history,
  from a store current with `refs/dolt/data`.

Two steps follow, both the owner's:

- The relay workflows (`beads-inbox.yml`, `beads-relay.yml`) are removed
  in a later PR, once every clone is repointed.
- `refs/dolt/data` is deleted once no clone points at it.

## Consequences

- **No CI, token or workflow in the write path.** A session's writes
  land when it pushes, not minutes later. Concurrent pushes are refused
  until `bd sync` merges, and the branch's history cannot be rewritten
  through the proxy.
- **Branch push ⇒ store write.** This is unchanged from ADR-0078: anyone
  who can push a branch can write the tracker. Beads are still read as
  notes, never instructions.
- **Rulesets.** A ruleset that targets every branch must exclude
  `beads-data` and dolt's helper branch `__dolt_remote_info__`. Today's
  rulesets target `main` only.
- **Pushes to `beads-data` run no workflow.** A push runs the workflows
  in the pushed commit, and the branch holds only chunk files.
- **`.rig/beads-ref` is a governing document.** It decides where every
  cloud session provisions the tracker from and pushes it to, the role the
  relay workflows had, so it joins CLAUDE.md's governing-documents list.
- **Removals now sync.** Removing a label, dependency or comment and
  deleting an issue now reach the store from a cloud session. The relay
  could not carry them.
