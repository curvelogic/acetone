# ADR-0077: Stale-lock robustness — enforce daemon-exclusivity at startup

*Status: accepted — ruled by Greg at the Phase 12 opening (2026-08-11),
selecting option (a) of the three recorded on the decision bead · Date:
2026-08-11 · Bead: acetone-pz0k.8*

## Context

The daemon's stale-writer-lock recovery (ADR-0074 §8, PR #269) —
unlink-then-`O_EXCL`-recreate, serialised by an in-process mutex — is
safe **only** under the one-daemon-per-repository model. Two daemons on
one repository (different sockets) share no mutex, and `remove_file` is
unconditional, so concurrent recovery can reopen the double-writer
window. File locks offer no race-free cross-process "break only if
still stale" primitive, so a cleverer break cannot fix this; running
two daemons on one repository has been documented UNSUPPORTED since the
Phase 11 boundary. The options recorded on `acetone-pz0k.8`:
(a) enforce daemon-exclusivity with an exclusive lock at `serve`
startup; (b) replace the `O_EXCL` lock file with a stale-immune
flock/fcntl advisory lock the kernel drops on process death;
(c) harden the documentation only.

## Decision

**Option (a): daemon-exclusivity is enforced, not assumed.** At `serve`
startup the daemon takes a per-repository exclusive advisory lock
(kernel-released on process death, so a crashed daemon never wedges the
next one) and holds it for its lifetime. A second `serve` on the same
repository fails fast with a clear error naming the running daemon.
The existing stale-writer-lock recovery then remains sound as shipped,
because the one-recoverer premise it depends on is now guaranteed by
the exclusivity lock rather than by operator discipline. This is the
smallest and most portable option and matches the host-owns-the-
repo-pool model (ADR-0072): a host that wants N daemons runs N
repositories.

Option (b) is not pursued: it would replace working, reviewed recovery
machinery wholesale for a portability-sensitive primitive, to solve a
configuration the model already rules out. Option (c) alone leaves the
window open to accident.

## Consequences

- `acetone-pz0k.7`'s scope is fixed: the startup exclusivity lock, the
  pid-reuse refinement (process identity + start-time, not bare pid),
  and routing the `schema-apply`/`import` write paths through the typed
  recovery helper.
- The "two daemons on one repo" caveat leaves the documentation once
  the refusal ships — the failure mode becomes an error message, and
  gate criterion 3 of ADR-0075 tracks it.
- The CLI's own writer-lock behaviour is unchanged: CLI processes are
  transient writers arbitrated by the existing lock file; the
  exclusivity lock governs daemons only.
