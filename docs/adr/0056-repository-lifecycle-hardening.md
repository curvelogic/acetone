# ADR-0056: Repository lifecycle hardening — recoverable checkout, read-only open, no-change commit guard

*Status: accepted — taken mid-phase per the working protocol, for retrospective review at the next boundary · Date: 2026-07-23 · Beads: acetone-tqd, acetone-ayq, acetone-k78*

## Context

Three review findings against the repository lifecycle (0.3.x quality pass,
epic acetone-7qw) share a shape: an operation's contract was under-specified
at its edges — crash windows, read paths that write, commits that record
nothing.

1. **Checkout is two ref updates** (acetone-tqd, PR #19 finding): the
   workspace ref CAS lands, then `set_head` moves the checked-out ref. A
   crash between them left workspace = target content while HEAD still named
   the old branch; `is_dirty` then read true and *every* subsequent checkout
   refused (`DirtyWorkspace`) — wedged until manual ref surgery.
2. **`open()` wrote on a read path** (acetone-ayq, rjf review finding): the
   first open of a fresh linked worktree bootstrapped its workspace ref under
   the writer lock. A read-only command (`log`, `status`) could fail with
   `Locked` against a concurrent writer, and failed outright on a read-only
   filesystem — contradicting "readers never touch this lock".
3. **`commit` minted no-change commits** (acetone-k78, PR #20 mandatory
   finding): repeated `acetone commit -m x` on a clean workspace created a
   fresh commit each time; the CLI grew a client-side `is_dirty()` stopgap
   that every other embedding would have had to duplicate.

Constraint: `Repository`, `Transaction`, `Snapshot` and `GraphError` are in
the frozen 0.2 API surface (ADR-0046, STABILITY.md) — within 0.3.x, changes
must be additive; no signature changes.

## Decision

**Checkout — keep workspace-first ordering; make retry the recovery.** The
two-step order (workspace CAS, then head) is now a documented contract, and
the dirty guard is narrowed to "the checkout would change workspace content":
when the branch exists and the per-worktree workspace ref already resolves to
exactly the target's committed manifest, checkout skips the guard and the CAS
and just moves the checked-out ref. Re-running the interrupted checkout is
therefore the idempotent recovery; checking out any *other* branch from the
interrupted state still refuses. Workspace-first is deliberate: from its
interrupted state, `commit` records the target's committed content onto the
*old* branch — safe and explicit — whereas head-first's interrupted state
would let `commit` silently write the old branch's content onto the *new*
branch. A journal was rejected as over-engineering for a single-writer
embedded store where idempotent retry suffices.

**Open — read-only; the workspace becomes virtual until first write.** The
first-open bootstrap is deleted. The effective workspace is now layered:
per-worktree ref → legacy pre-ADR-0014 ref → *virtual* (the checked-out
commit's committed manifest, resolved read-only — detached HEAD included,
preserving acetone-cm9). The first write materialises the per-worktree ref
via the existing expected-`None` CAS create, always under the writer lock, so
there is no provisioning race. Supporting change: `GitStore` gains
`commit_manifest_id` (the manifest blob id read from the commit tree), and
`Repository::commit_manifest_hash` uses it instead of re-`put`ting the
manifest bytes — removing the last write from every read path.

**Commit — refuse no-change commits; empty commits are an explicit opt-in.**
`Transaction::commit` (signature unchanged) now returns the new
`GraphError::NothingToCommit` when, after staged writes apply, the workspace
manifest equals the checked-out commit's — or, on an unborn branch, is still
the blank init state. The check is manifest-level, so operations that net to
no change are refused too. Merge completions are exempt (they record
two-parent history). The additive opt-in `Transaction::commit_allow_empty`
(the `git commit --allow-empty` analogue) serves deliberate marker commits;
the CLI exposes it as `acetone commit --allow-empty` and drops its
client-side stopgap. Alongside (security review LOW-2), `Snapshot` gains
streaming `node_count`/`edge_count`/`schema_entry_count` and `acetone status`
stops materialising every record just to count.

## Consequences

- **No wedged states.** Every crash window in checkout leaves a state that
  either reads normally or is recovered by re-running the same command;
  proven by tests that construct the interrupted state and drive recovery.
- **Reads are reads.** `open` and every read-only command work on a
  read-only filesystem and cannot contend with a writer (tested by opening a
  fresh worktree while the writer lock is held, and on a `chmod -w` tree).
  A fresh worktree no longer gains a workspace ref until something writes —
  fsck and gc see one ref fewer until then, and nothing uncommitted exists
  for a foreign gc to lose.
- **Commit histories carry intent.** A commit now always records either a
  content change or an explicitly requested marker. Callers that relied on
  empty commits must opt in (property-test harnesses in-repo did; updated).
- **Frozen-surface note:** the change is additive (two new methods, three new
  counters, one new error variant) except that `GraphError` gains a variant —
  exhaustive downstream matches on it would need a new arm. The curated
  `public-api.txt` (a re-export list) is unchanged. Flagged for the phase
  report.
- **Foreclosed:** nothing. A journal (or git-style ref transaction) can still
  be layered under checkout later if multi-ref updates multiply; the virtual
  workspace does not preclude eager provisioning tools.
- **Revisit if:** checkout ever grows to three or more refs (journal then),
  or an embedding legitimately needs empty commits often enough that the
  opt-in becomes friction.
