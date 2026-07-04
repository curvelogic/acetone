# Phase 0 milestone security review — summary

*2026-07-04 · Fresh security subagent over the whole Phase 0 diff (PRs #1–#6)
plus live repository configuration. Verdict: GATE-READY — no blocker-class
findings. Full trail in the session record; dispositions below.*

## Live infrastructure — verified good

Action SHA pins authentic upstream commits; workflow token read-only; no
secrets in CI; no `pull_request_target`; deny.toml sound (unknown
sources/registries denied, yanked denied); root `cargo deny` green;
`cargo audit` over the spike's gix 0.85 tree: 181 dependencies, zero
advisories.

## Findings and dispositions

| # | Severity | Finding | Disposition |
|---|---|---|---|
| S1 | should-fix | No branch protection on `main` — the merge gate was policy, not mechanism | **Done**: ruleset `protect-main` active — PR required, the four CI checks required, force-push and deletion blocked |
| S2 | should-fix | Repo default workflow token was `write`; Actions could approve PRs (a hole in reviewer sign-off) | **Done**: defaults set read-only; Actions PR approval disabled |
| S3 | should-fix | Spike manifest on `main` had a duplicate `[workspace]` table (PR #5 × PR #6 squash artefact) — spike unbuildable from `main`, and CI cannot see it | **Fixed** in the Phase 0 close-out PR; the spike-outside-CI trade-off is accepted consciously (below) |
| S9 | should-fix | Hardening carry-overs claimed as "recorded on beads" were not (silent `bd comments` syntax failure) | **Done**: hostile-repo checklists now verified present on acetone-63m.1, 63m.2 and 63m.7 |
| S5–S8 | should-fix (Phase 1) | Hostile-repo gaps beyond the known list: attacker-controlled `count` drives `Vec::with_capacity` (~200 GiB reservation from a 10-byte chunk); manifest chunk-params unvalidated (`mask_bits ≥ 64` shift panic, height/level truncation); no ordering/consistency validation — hostile sortedness violations yield **silently wrong answers**, breaking history independence and merge determinism as integrity properties; unbounded blob reads and default-trust repo opening (repo-local config honoured; bench shells `git -C` against the repo) | Recorded as normative requirements on acetone-63m.1/63m.2, enforced via fsck (63m.7) with corrupted-input corpora |
| S4, S10, S11 | informational | rust-cache branch scoping acceptable; `.beads/hooks`/`.codex`/`.claude/settings.json` execute on Greg's machine when touched by merges — propose treating these paths as governing-document-class in review; CI runs repo code on PRs with read-only token (re-check if repo goes public) | S10 proposal queued for Greg in the phase report |

## Consciously accepted trade-off

The spike (including its gix dependency tree) is outside CI — build, test
and dependency scanning. That is the price of workspace exclusion for
throwaway code, accepted for Phase 0 only; when gix enters workspace crates
in Phase 1 it automatically comes under the deny job and CI.

## Phase 1 hostile-repo checklist

The normative version lives on beads acetone-63m.1 (store: ref-name
validation, reduced-trust repo opening, bounded blob reads, no shelling out
to git) and acetone-63m.2 (prolly: no panics on decode, allocation bounded
by input not declared counts, strict manifest validation, structural
validation so corrupt data is `Corrupt` rather than wrong answers,
writer-side length limits), enforced through fsck (acetone-63m.7).
Threat model: every chunk, manifest, commit and ref may have been written by
an attacker — hostile clones are within the product's normal workflow.
