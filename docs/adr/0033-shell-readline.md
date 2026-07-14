# ADR-0033: Adopt `rustyline` for the interactive shell

- Status: accepted
- Date: 2026-07-14
- Deciders: agent under the 0.1.1 autonomous mandate; ratified by Greg at the 0.1.1 boundary (2026-07-14)

## Context

`acetone shell` read input with a raw `stdin.read_line` loop. It had no line
editing: arrow keys emitted their raw escape sequences straight into the Cypher
parser (so "up-arrow then Enter" produced a parse error rather than recalling
the previous query), there was no command history, and none of the standard
readline motions (Ctrl-A/E/K/W, reverse-search) worked. This was the single
most-cited daily-driver friction in the 0.1.1 dogfooding review (bead
acetone-86i).

A line-editor is not something to hand-roll: correct terminal handling across
platforms, key bindings, and history are a large, well-trodden surface.

## Decision

Adopt the **`rustyline`** crate (v15, default features) for the interactive
REPL, confined to `acetone-cli`.

- `rustyline` is the conventional Rust readline-alike: mature, widely used,
  actively maintained, MIT-licensed, and clears `cargo-deny`/`cargo-audit`.
- Only `acetone-cli` takes the dependency; the library crates
  (`acetone-model`, `-prolly`, `-store`, `-graph`, `-cypher`) stay
  dependency-clean, consistent with the workspace's layering discipline.
- The shell uses `rustyline` **only when stdin is a terminal**
  (`std::io::IsTerminal`). When stdin is piped — a script, `acetone shell <
  file`, or a test — it falls back to plain line reading with no editing or
  history, so the shell stays scriptable and testable and the interactive
  dependency never affects non-interactive behaviour.
- History persists to `$HOME/.acetone_history` with a bounded size.

`reedline` (nushell's line editor) was considered and rejected: it is heavier
and oriented at building a full shell, more than a Cypher REPL needs.

## Consequences

- New runtime dependency on `rustyline` (and its transitive deps) in the
  shipping `acetone-cli` binary. Justified per the CLAUDE.md dependency policy;
  the maintenance/licence/security posture is good and CI vets it each build.
  One transitive dependency (`error-code`) is under **BSL-1.0** (Boost Software
  License 1.0), which was added to the `deny.toml` licence allow-list — it is
  permissive, OSI-approved and MIT/Apache-2.0 compatible.
- The interactive line-editing path cannot be unit-tested without a PTY. The
  non-TTY fallback path — which shares the same statement-accumulation and
  meta-command logic — is fully covered by piped-stdin integration tests, so
  the REPL's behaviour (multi-line statements, meta-commands, EOF) is tested
  even though the raw terminal editing is not.
- Ctrl-C now cancels the current partial statement and returns to a fresh
  prompt (rather than killing the process); Ctrl-D exits.
- If a future frontend (library/agent) needs a REPL, it should reuse the
  statement/meta logic rather than the terminal layer, which is CLI-only.
