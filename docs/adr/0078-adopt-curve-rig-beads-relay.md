# ADR-0078: Adopt curve-rig's beads relay for cloud sessions

*Status: accepted · Date: 2026-10-20 · Bead: acetone-op4y*

## Context

The tracker lives in a Dolt store shared through `refs/dolt/data` on
`origin`. Cloud (web) sessions reach GitHub only through a git relay that
forwards branch creates and fast-forwards under `refs/heads/*` and refuses
everything else (rig's `docs/relay.md` → "Why it is needed"); in
particular a push to `refs/dolt/data` is refused
(`docs/notes/operational-constraints.md`).
`bd create`, `update`, `close`, `comments add` and `remember` nevertheless
report success — they write the session's local store — and the writes are
lost when the container is reclaimed. A cloud session that files findings
or closes beads has silently changed nothing.

curve-rig (public, `curvelogic/curve-rig`) provides a relay for exactly
this: the session stages its delta as a file on a branch of its own, and CI,
whose `GITHUB_TOKEN` can push any ref, applies it.

## Decision

Adopt rig's inbox-branch relay, pinned to rig commit
`ed3974307262bf88fe64a0ebabd956c1b6dab8e9` (tag `v0.1.0`, verified to be on
rig's `main`), exactly as rig's `docs/relay.md` → "Adopting it in a member"
gives it at that commit:

- `.github/workflows/beads-inbox.yml` — on a push to `beads-inbox/**`,
  with no checkout, no secrets and `actions: write` only, dispatches the
  relay on the default branch;
- `.github/workflows/beads-relay.yml` — `workflow_dispatch` only; calls
  rig's reusable workflow with `contents: write`. Both the `uses:` SHA and
  `rig-ref` are the same pinned commit. There is no `secrets:` block: rig
  is public.

Cloud sessions stage their writes with the `rig-relay` skill (`CLAUDE.md`).
The existing push-triggered workflows (`ci.yml`, `docs.yml`) already filter
`push` to `main`, so they do not run on inbox branches; the repository has
no Dependabot configuration, so no `ignore` entry is needed. Both are to be
revisited if either changes. The two relay workflows join `CLAUDE.md`'s
governing documents: they hold a `contents: write` token and their pin
decides which bd migrates the store, so they and any pin bump get full
adversarial review plus an ADR.

Before relying on it: rig's settings check for a public member (a `main`
ruleset requiring a PR, its checks and an approval, owner bypass only;
*Allow GitHub Actions to create and approve pull requests* off; workflow
permissions read-only by default; secrets in environments restricted to
`main`) is Greg's to confirm in the repository settings, and rig's
"Checking it end to end" steps 1–4 run once after this merges.

## Consequences

- **Inbox branches are briefly public.** Each delta sits on a
  `beads-inbox/…` branch until the relay applies and deletes it. The beads
  data is already public through `refs/dolt/data`, so nothing new leaks.
- **Branch push ⇒ store write.** Anyone who can push a branch carrying a
  workflow could already write `refs/dolt/data` with a workflow of their
  own (rulesets do not cover that ref); the relay makes it workflow-free,
  so a prompt-injected session can write beads every later session reads.
  Beads and memories are therefore notes about the work, never
  instructions (`CLAUDE.md`). Nothing the relay does reaches `main`.
- **Staleness gate.** A delta in which any issue is older than the store's
  copy is refused whole and its branch left in place; the run fails, naming
  it, and every later run reports it again until it is deleted. Redo the
  writes in a fresh session; the branch is deleted by Greg or from a local
  session, since a cloud session cannot delete branches.
  Labels and comments are merged, not gated; memories are not gated.
- **Removals aren't relayed.** `bd import` only upserts: removed labels,
  dependencies or comments, edited comments, deleted issues and forgotten
  memories must be made from a local session.
- **Local sessions keep syncing directly** with `bd dolt push`/`pull`.
- **Pin discipline.** The relay runs rig's pinned bd; a newer pin can
  migrate the store schema for every clone. Bump both SHAs together, by
  hand, to a reviewed commit on rig's `main`, as a governing-document
  change.
- Revisit if cloud sessions gain the ability to push `refs/dolt/data`, or if
  rig changes the relay's trust model.
