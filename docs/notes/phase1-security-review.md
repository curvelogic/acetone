# Phase 1 milestone security review

*2026-07-04 · dedicated phase-boundary security pass (fresh subagent, no
implementation context) over the whole Phase 1 diff `88a03a6..origin/main`.
Required by CLAUDE.md at each phase's end. This file is the reviewer's
report verbatim, followed by the disposition of each finding.*

## Disposition summary (added post-review)

Verdict: **no blocker.** All findings dispositioned before the Phase 1
report:

- **HIGH-1** (`acetone log` terminal injection) and **MEDIUM-1** (`acetone
  fsck` output) — fixed in PR #25 (bead acetone-bwb). The per-PR review
  of #25 found and the fix closed a third sink of the same class
  (`get-node` secondary labels).
- **LOW-1** (CBOR array preallocation amplification) — bead acetone-8gp.
- **LOW-2** (`status` materialises all records to count) — folded into
  bead acetone-k78.
- **Residual** (bidi/zero-width visual spoofing) — bead acetone-0ds.

None touches the load-bearing storage invariants.

---

## Reviewer's report

Cross-cutting security pass over the whole Phase 1 diff
(`88a03a6..origin/main`, tip `220692f`), reviewed from an isolated
worktree of origin/main. This is the dedicated milestone review CLAUDE.md
requires; per-PR adversarial reviews already ran, so this focuses on
seams between crates, systematic classes, and DoS.

### Verdict

The storage core is in very good shape. Every untrusted-input decoder I
examined (CBOR reader, value/record/manifest/node/key decoders, pack-index
parser) is strict, total, allocation-bounded and panic-free, with the
trust boundaries re-validated at each layer. I found **no BLOCKER**. The
gate is fit to close on the storage invariants once the one HIGH is
dispositioned.

The single substantive finding is an **unfixed instance of the
terminal-injection class PR #20 was created to fix**: `acetone log`
renders attacker-controlled commit subjects and trailers to the terminal
without escaping.

### Findings

**HIGH-1 — `acetone log` prints hostile commit subject and trailers
unescaped (terminal injection).** `crates/acetone-cli/src/commands.rs`
`log()` prints `subject` (first line of the commit message) and each
trailer `key`/`value` via bare `println!`, with no escaping. Those
strings originate in `acetone-store` `read_commit` as
`String::from_utf8_lossy(...)` of raw commit bytes — arbitrary content on
a clone of a hostile origin (git constrains commit *headers*, not the
message body or trailer text, so ESC/ANSI/C1 bytes pass through). Attack:
a hostile repo commits a subject/trailer containing ANSI escape
sequences; the victim runs `acetone log` and the sequences hit their
terminal (output spoofing — forged/hidden commits — and, on some
emulators, worse). This is exactly the class PR #20 neutralised for
labels/keys via `format_label`/`format_value` (`{:?}` escaping); `log()`
was missed, so the mitigation is incomplete. Rationale for HIGH not
BLOCKER: it does not touch any storage/format/merge invariant, and impact
is terminal-dependent; but it is reachable via a core command on
baseline-hostile input and defeats part of an already-shipped mitigation,
so it should be fixed before the CLI is claimed hostile-safe.

**MEDIUM-1 — fsck `Finding`/`Origin`/`MapId` Display embed
repository-controlled strings unescaped (latent terminal injection).**
`crates/acetone-graph/src/fsck.rs`. `MapId::Index(name)` interpolates the
manifest-supplied index name, and `Finding.detail` embeds decode-error
text and index names; `Origin` embeds ref names. Index names are
attacker-controlled UTF-8 (the manifest decoder enforces
non-empty/ascending/UTF-8 but *not* absence of control characters). It
becomes live terminal injection the moment `acetone fsck` is wired
(acetone-63m.6). Fix: escape repository-controlled fields at the Display
boundary, or require the CLI to escape before printing.

**LOW-1 — `Vec::with_capacity(count)` memory amplification in array/list
decoders.** `crates/acetone-model/src/values.rs` (`read_value`,
MAJOR_ARRAY) and `crates/acetone-model/src/records.rs` (secondary-labels
array). `count` is correctly pre-checked against `reader.remaining()`, so
it is bounded by input bytes — but the reservation is
`count * size_of::<Value>()` (~32 B) / `count * size_of::<String>()`
(24 B), a ~24–32× amplification. A ~60 MiB hostile chunk (under the 64 MiB
object cap) whose value is an array of that many single-byte ints reserves
~2 GiB transiently while decoding one value, which could OOM a
memory-constrained node. Bounded and single-shot, but a real amplification
over the object cap. Fix: cap the preallocation and let it grow.

**LOW-2 — `acetone status` fully materialises all nodes and edges just to
count them.** `crates/acetone-cli/src/commands.rs` calls
`snapshot.nodes()?.len()` and `snapshot.edges()?.len()`, each of which
collects every record into a `Vec`, then discards it for a count. A
streaming `Snapshot::count` already exists and is used by `summarise`.
Inefficiency/DoS-on-scale, not a correctness bug.

### Checked-OK (coverage)

- **Cross-crate untrusted-input totality.** The seam chain manifest→Root→
  prolly walk→store read is sound. `Manifest::decode` bounds height to
  `1..=MAX_HEIGHT` (64) and validates hash width/params; `Root::new`
  re-validates height, so allocation-and-recursion in `apply_batch` is
  bounded to ≤64. Every prolly read re-applies position checks (level tag,
  parent boundary claim, sibling lower bound), so a chunk valid in
  isolation but referenced from the wrong position yields `Corrupt`, not a
  wrong answer. `commit_manifest_hash` re-adds un-decoded manifest bytes
  but only ever content-addresses them; interpretation always goes through
  the strict decoder first. No assume-the-caller-checked seam found.
- **CBOR / key / record decoders.** Shortest-form heads, definite lengths,
  depth limit, declared lengths pre-checked against remaining input;
  strict decode, allocation against actual input, typed errors not panics.
  `i16::MIN` offset and NaN/-0.0 canonicality edge cases tested.
- **Path/ref injection.** `validated_ref_name` is the single door
  (requires `refs/`, then gix `FullName::try_from`, which rejects ASCII
  control chars). `list_refs`, `set_head`/`read_head`, and the workspace/
  branch prefixes all route through it. Sidecar/lock/pack files use fixed
  names joined onto `common_dir`; pruning derives paths from validated OID
  hex. The single-writer lock uses `O_CREAT|O_EXCL` with a fixed filename.
- **Panics/DoS on untrusted data.** The consolidation reachable walk, fsck
  walks, `Plan::build` cycle-break and `cap_chains` are all iterative
  (de-recursed for deep hostile histories, 100k-link regression test).
  Diamonds expand once. fsck memoises `(root,height)` and manifest hashes
  across versions (mitigating acetone-7fe). The 64 MiB object cap is
  enforced on every read path including the consolidation walk, checked
  from the object header before materialisation. `idx_oids` uses
  `checked_mul`/`checked_add`. Pack varint/delta code is writer/
  self-validation only; incoming packs are parsed by gix.
- **Terminal injection elsewhere.** `format_label`/`format_value`
  correctly escape labels, keys, property names/values and relationship
  types throughout `commands.rs`. `StoreError`/`GraphError` Display
  interpolate ref names via `{name:?}`. The gaps are HIGH-1 and MEDIUM-1
  only.
- **Dependencies & subprocess.** New this phase: `clap` (derive) and
  `anyhow` at the CLI (justified, dual MIT/Apache-2.0); `flate2` (pinned
  to the `zlib-rs` backend gix already compiles) and `crc32fast` (already
  transitive) at the store. `deny.toml` covers advisories/licences/
  sources/bans; CI runs `cargo-deny check` (RUSTSEC DB) plus fmt, clippy
  `-D warnings`, build and test, with actions pinned to SHAs. gix is
  compiled with process-spawning/network/credential/filter features
  disabled and opened `isolated` + `Trust::Reduced`; consolidation shells
  out to nothing (the only `Command::new("git")` is in `#[cfg(test)]`).
- **Secrets/credentials.** Nothing in the phase touches credentials. No
  remote round-trip code exists in the phase (push/fetch use the ambient
  git tooling outside these crates). I found no e2e/exit shell script in
  the phase diff — the only shell-executable files are the
  `.beads/hooks/*` git hooks, which are beads-managed and not part of this
  diff, so out of scope here.

  *(Disposition note: the review ran at tip `220692f`, before PR #24
  merged `scripts/phase1-e2e.sh`; that script was reviewed under PR #24,
  including its `git fsck` assertions and the remote round-trip block,
  and uses only the ambient git credential setup.)*
