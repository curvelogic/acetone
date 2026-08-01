# ADR-0067: Explicit-instruction delegation of Greg-reserved acts

*Status: accepted — Greg's ruling at the post-0.4.0 direction discussion (2026-08-01): "make the amendment — my explicit instruction wins" · Date: 2026-08-01 · Bead: acetone-zvub*

## Context

The governing documents reserve two acts for Greg personally: closing the
bead that gates a phase boundary (CLAUDE.md, Autonomous Working Protocol and
Branch & Merge Discipline), and publishing a release ("No agent publishes a
release" — ADR-0057 and the release formula's human gate; `docs/RELEASING.md`
step 4). The reservations exist because both acts are approvals: the gate
close ratifies a phase's exit evidence, and the publish mints the `v*` tag
and ships binaries.

Across the Phase 9 boundary review and the 0.4.0 release — one session, two
occasions (the gate close on 2026-07-31, the publish on 2026-08-01) — Greg,
reviewing on mobile and satisfied with the evidence, explicitly instructed
the agent to perform each act, separately. The agent did so, recording each
instruction contemporaneously on the beads rather than normalising it
silently, and flagged the docs-versus-practice divergence for the next
boundary. This ADR resolves that flag.

## Decision

**An explicit, informed, current instruction from Greg authorises an agent to
perform one of the two acts the governing documents reserve for him —
closing a gate/exit-criteria bead, or publishing a release — and no
others.** Extending the delegable set is itself a governing-document change
requiring Greg's agreement; in particular this ADR does not touch the
prohibitions that are requirements rather than reservations (the adversarial
review gate, never committing directly to `main`, never force-pushing it).
The approval is still Greg's — the agent is executing his decision, not
making one. Conditions, all required:

- **Explicit**: the instruction names the act ("publish the release", "close
  the gate bead") and is received **first-hand from Greg in the current
  session** — a relayed or reported instruction (via another agent, a
  subagent brief, or a bead) is not a delegation and must be confirmed with
  Greg directly. Ambiguity or silence is never consent; a general mandate
  to work autonomously does not cover reserved acts.
- **Informed**: Greg has seen the evidence the act approves (the verified
  draft, the gate evidence) or has been told plainly what he is approving.
- **Current**: the instruction covers this occasion only. A past delegation
  does not carry forward; the next release or boundary starts from the
  reservation again.
- **Recorded**: the agent records the instruction verbatim where the act is
  recorded — the bead close reason; for a release, the publish step's bead —
  so the audit trail shows delegated-by-instruction, not agent-initiated.

The reservations themselves stand unchanged in every other respect: absent
such an instruction, no agent closes a gate bead or publishes a release.

## Consequences

- CLAUDE.md gains an "Explicit-instruction delegation" gate bullet and its
  two reserved-act rules are amended to reference it; `docs/RELEASING.md`
  step 4 and the release formula's publish gate are aligned; ADR-0057's
  status line notes the amendment.
- The 0.4.0-cycle delegations were explicit, informed and current, and
  recorded **in substance though not verbatim** (both bead records paraphrase
  Greg rather than quoting him) — which is precisely why this ADR requires
  verbatim recording rather than treating the precedent as fully regular.
- Agents may state that delegation is possible when Greg asks how to proceed,
  but must not solicit it as a convenience ("shall I just publish?") — the
  default path remains Greg acting himself.
