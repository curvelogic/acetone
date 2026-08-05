# ADR-0072: Incorporating the Phase 10 tenant's feedback into the roadmap

*Status: accepted (agent decision on Greg's instruction to review the
tenant's requests "and update our roadmap to incorporate what makes
sense"; the specific incorporations are flagged for Phase 10 boundary
review as a governing-doc change) · Date: 2026-08-05 · Bead: acetone-tznd*

## Context

Phase 10's criterion 4 gives acetone its first real tenant: a private
external application embedding acetone as its fact store. The tenant
consolidated its friction and requests into a tiered document
(deliberately framed so that every ask is something a *second* tenant
would also want), and asked four cheap verification questions. The
verifications were answered from the code with no work needed — reader
concurrency is already a documented MVCC guarantee; parallel edges
already ship as relationship-type discriminators; one graph per
repository is a hard limit as shipped; and pushing workspace refs for
cross-machine resume is *designed against*, which corrected a plan the
tenant had considered settled (the supported shape is commit to a scratch
branch and push that).

That left the genuine roadmap questions: which asks change the plan, and
how.

## Decisions

1. **Daemon mode: direction accepted; the roadmap's server-mode line is
   reshaped around it.** The prior line reserved "an optional read-only
   server mode for dashboards". The tenant's shape is better and is
   adopted as the target: a **per-repository daemon (`acetone serve`)
   that owns nothing** — no auth, no tenancy, no repo pool, no
   credentials, no transport policy; the host hands it a directory and
   owns everything else. This preserves the workbench identity exactly
   because the daemon is the CLI generalised, not a server product; it
   is the natural embedding surface for non-Rust hosts; and it subsumes
   the "stable machine interface" ask (the CLI's `--json` is explicitly
   unstable pre-1.0). The read-only dashboard mode becomes a special
   case. **Timing is unchanged: deliberately late** — the scheduling
   signal the tenant asked for is "accepted direction, not before the
   post-0.5 phases, sequenced at a boundary by Greg".
2. **Relationship-type rename/merge joins schema evolution, and the
   ratchet argument is accepted.** Rel-type autodeclare (ADR-0060) makes
   vocabulary growth one-way: near-duplicate predicates accumulate and
   only a rename/merge repair can heal the graph — coinage-time
   fuzzy-match warnings are the embedder's mitigation, not a repair. The
   schema-evolution roadmap line (previously "rename label/property")
   now names rel-type rename/merge explicitly, and it should land
   **before a second autodeclare tenant ships**. Backlog bead filed.
3. **Two absent items join the unscheduled list**: (a) progress
   reporting and resumable chunking for long imports and merges
   (workspace refs already make the state durable; the ask is
   observability); (b) **multi-graph co-tenancy** — the namespace
   machinery already generalises to *n* graphs and gc/migrate ownership
   is namespace-scoped, so the residual gate is `open`'s one-graph
   refusal, fsck's namespace-blindness, and per-graph workspace refs
   (the acetone-42d family). Recording it makes the existing design
   intent visible instead of incidental.
4. **Tenant pull is recorded on two lines already present** — blame
   surfacing over graph history and structured change-report export —
   so future sequencing sees demand, not just supply.
5. **Considered and not adopted: transferable workspace state.** The
   tenant's session-mobility need is met by committing to a scratch
   branch and pushing that — the design's transferable-state contract
   (`refs/heads|tags` only) stands. A first-class "portable session"
   feature would cut against the workspace's correct-to-lose semantics
   and the operational constraint that proxies reject custom ref
   namespaces; it is declined unless a tenant demonstrates a need that
   commit-and-push cannot meet.
6. **No roadmap change for the already-shipped or already-guaranteed**:
   reader concurrency (documented MVCC), parallel edges (rel-type
   discriminators; `schema apply` declares them — the imperative CLI
   deliberately cannot), typed edge qualifiers, wasm and watch/reactive
   queries (the tenant itself asked these stay unscheduled; watch was
   already listed).

## Consequences

- The roadmap's closing section changes (this ADR's companion edit):
  the daemon line is reshaped, schema evolution names rel-types,
  import/merge observability and multi-graph co-tenancy are added, and
  tenant-pull markers appear on blame and change-report export.
- Backlog beads exist for the daemon direction, rel-type rename/merge,
  import/merge observability, and multi-graph co-tenancy, so boundary
  sequencing has concrete handles.
- As a governing-document change this ADR and the roadmap edit go
  through full adversarial review and are listed prominently in the
  Phase 10 report for Greg's boundary review.
