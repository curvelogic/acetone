# ADR-0080: Refer to the owner by role; a phase opens on the owner's instruction

*Status: proposed — a governing-document change for the owner to merge ·
Date: 2026-10-25*

## Context

`CLAUDE.md` named the repository's owner throughout. The repository is
public, and the owner asked not to be named: an agent needs to know whose
decision a thing is, not who that person is.

The Phase start rule said agents implement only after the owner has
"explicitly opened the phase", but not how an opening happens or where it
is recorded. One session read that as needing a PR to accept the plan's ADR
and amend the roadmap (#298, closed unmerged). The owner's view is that
opening a phase means starting an agent.

## Decision

1. `CLAUDE.md` refers to the owner by role, "the owner". Text that
   carried no instruction is cut: the dated attributions on the sprint
   demo, the subagent tiers and the Phase start rule, and the approval
   history of the governing-document list. No other rule changes meaning.
2. A phase opens when the owner tells an agent session, first-hand, to
   open it, or confirms the opening first-hand in that session's thread.
   That session records the instruction verbatim on the phase epic bead
   and starts work. Opening needs no PR, ADR or roadmap change. The plan's
   ADR and roadmap section, merged in the plan PR, stand as merged, and the
   phase report records the opening. A gate close or a ratified design
   decision is still not an opening.

## Consequences

- Agents open a phase without public ceremony; the epic bead and the phase
  report are its record.
- A plan ADR may stay at *proposed* after its phase opens; the epic bead
  and the phase report, not the ADR's status, show that it is open.
- Other tracked files (ADRs, reports, demos) still name the owner. They
  are historical records and are not changed here.
