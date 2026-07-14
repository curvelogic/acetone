# ADR-0034: Discover the repository from a subdirectory

- Status: accepted
- Date: 2026-07-14
- Deciders: agent under the 0.1.1 autonomous mandate; ratified by Greg at the 0.1.1 boundary (2026-07-14)

## Context

`Repository::open` calls `gix::open_opts(path, …)`, which opens a repository
**only** if `path` is itself the repository root (or git dir). It does not
walk up parent directories the way `git` does. So `acetone status` run from
`repo/some/deep/dir` fails with "no acetone workspace", even though the
enclosing `repo` is a perfectly good acetone repository — you must either `cd`
to the root or pass an exact `--repo`. This was recurring friction in the
0.1.1 dogfooding review (bead acetone-7bn.12).

The `--repo` flag defaults to `.` (the current directory).

A prior concern was whether upward discovery could breach the store's
**isolated-store boundary**. Investigation shows it does not: that boundary is
*configuration* isolation — the store opens every repository with
`gix::open::Options::isolated().with(Trust::Reduced)`, so a repository's git
config, hooks and external commands are never loaded or run, even on a clone
of hostile origin. That posture is a property of *how* a repository is opened,
entirely independent of *which* directory is opened. Discovery changes only
the latter.

## Decision

`Repository::open` **discovers the enclosing repository by walking up** from
the given path (default: the current directory), opening the nearest ancestor
that is an acetone/git repository — matching `git`'s and `git -C`'s ergonomics.

- **Isolation is unchanged.** The discovered repository is opened with the
  same `isolated().with(Trust::Reduced)` options. Discovery never relaxes the
  trust posture; a repository found by walking up is treated exactly as one
  named explicitly.
- **Bounded.** The walk stops at the filesystem root and honours
  `GIT_CEILING_DIRECTORIES` (the same environment safety-valve `git` uses), so
  it cannot wander arbitrarily far up a shared filesystem.
- **`git -C` semantics.** Discovery starts from the `--repo` value (default
  cwd). An explicit `--repo` that already points at a repository root opens it
  immediately with no walking (so scripts that name a real root are exact and
  deterministic); an explicit `--repo` pointing inside a repository walks up to
  its root, as `git -C <subdir>` does.
- **`init` is exempt.** `acetone init` creates a repository at the exact path
  given and must not discover an enclosing one.
- **Clear failure.** When no repository is found up to the boundary, the error
  names the starting path and the boundary reached, and points at `acetone
  init`.

## Consequences

- `acetone` works from any subdirectory of a repository, matching muscle
  memory from `git`.
- The config-isolation security boundary is preserved verbatim: a repository
  reached by discovery still cannot run hooks or load config, so "discovering
  an unexpected parent repository" cannot execute anything — the worst case is
  operating on a repository you didn't mean to, bounded by ceiling dirs, and
  reported by `status`/the prompt (which show the branch and head).
- Scripts that pass an exact `--repo <root>` are unaffected (immediate open,
  no walk).
- One behavioural change: a command run in a non-repository directory that
  *has* a repository ancestor now succeeds against that ancestor rather than
  failing. This is the intended git-like behaviour; the ceiling-dir bound and
  the `status`/prompt branch display mitigate surprise.
