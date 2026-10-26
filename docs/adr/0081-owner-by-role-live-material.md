# ADR-0081: Refer to the owner by role in the remaining live material

*Status: proposed — a governing-document change for the owner to merge ·
Date: 2026-10-26*

## Context

ADR-0080 changed `CLAUDE.md` to refer to the owner by role and left every
other tracked file alone. Live material — files agents and contributors
still follow or edit — kept the owner's personal name: the roadmap,
`docs/RELEASING.md`, the release formula, a workflow comment, an
operational note, and comments and fixture values in code and tests. The
repository is public and the owner asked not to be named.

## Decision

1. Live material refers to the owner by role, "the owner". Dated
   attributions keep their dates ("the owner, 2026-07-21").
2. In code and tests only comments and fixture values change. Fixture
   values that happened to be the owner's name become a neutral value
   (`"ops-team"`); no assertion's meaning changes. No identifier,
   configuration value or permission (for example a GitHub login in a
   ruleset) is touched.
3. Historical records stay as written: ADRs, phase reports, demo decks,
   the CHANGELOG and review records (such as
   `docs/notes/phase0-security-review.md`). The vendored openCypher TCK is
   third-party text and is out of scope. Git history is not rewritten.

## Consequences

- The release formula and the roadmap are governing documents; their
  change is wording only and alters no step, gate, permission or rule.
  The workflow change is a comment.
- New live material should follow the same rule. This ADR states it; it
  does not add it to `CLAUDE.md`.
