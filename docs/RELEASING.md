# Releasing acetone

The shippable artefact for 0.1 is the **single static binary** (`acetone`),
distributed via GitHub Releases and Homebrew. The library crates stay internal
for now (see the last section).

## Cutting a release — you publish, that creates the tag

There is **no manual tagging**. The release version comes from the workspace
`Cargo.toml` (`[workspace.package] version`), and the tag `v<version>` is created
by GitHub *when you publish the draft release* — with you as the actor, which is
what the tag-protection ruleset (only you may create `v*`) requires.

The flow:

1. **Bump the version** in the root `Cargo.toml` (`[workspace.package] version`)
   and the `acetone-*` path-dependency pins in `[workspace.dependencies]` — on
   a branch, landed by PR with review per CLAUDE.md's branch discipline (never
   directly on `main`; the Release workflow builds whatever `main` points at,
   so dispatch it only after this PR has merged). `acetone --version` and the
   binaries then report it.
1b. **Write the changelog section.** Move the accumulated `## [Unreleased]`
   entries in `CHANGELOG.md` under a new `## [<version>] - <date>` heading (Keep
   a Changelog format). **This section is the release body** — the workflow
   reads it verbatim via `body_path` and fails if it is missing — so make it a
   human-readable, summarised changelog, not a commit dump. Add new entries
   under `[Unreleased]` as PRs merge so this step is just a rename.
2. **Build the candidate.** Run the **Release** workflow from the Actions tab
   (`workflow_dispatch`). It builds `--release` (the workspace
   `[profile.release]`: `strip`, `lto`, one codegen unit) for each target and
   attaches a `.tar.gz` + `.sha256` to a **draft** release named `v<version>`,
   targeting the exact commit it built:
   - `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` — statically
     linked against musl, self-contained (no libc dependency);
   - `aarch64-apple-darwin`, `x86_64-apple-darwin` — the platform binaries (a
     fully static binary is not supported on macOS).
3. **Review the draft** and its binaries in the Releases UI.
4. **Publish** when happy. GitHub creates the `v<version>` tag at the target
   commit. That publish is your approval — nothing is tagged before it.
5. **Homebrew follows automatically**: publishing triggers the
   **Homebrew bump** workflow, which opens a formula PR on the tap (see below).
   Review and merge that PR.

Each release archive also carries a signed **build-provenance attestation**
(SLSA, via `actions/attest-build-provenance` in the Release workflow), so
published binaries are verifiably built by CI:

```
gh attestation verify acetone-v<version>-<target>.tar.gz --repo curvelogic/acetone
```

Build a binary locally with `cargo build --release --bin acetone`.

## Tracked execution: the release formula

The flow above is also encoded as a beads formula —
`.beads/formulas/release.formula.toml` (ADR-0057) — a dependency DAG
(`preflight → prep → land → build → publish → post-publish`) whose steps point
back at the sections of this document. Instantiate it to run a release as
tracked, dependency-ordered beads:

```
bd mol wisp release --var version=<version>   # ephemeral run (recommended)
bd mol squash <molecule-root>                 # digest it when done
```

The `publish` step carries a human gate: the molecule parks until Greg
publishes the draft and the gate is resolved (`bd gate resolve`). This
document remains the narrative authority — the formula deliberately contains
pointers and acceptance criteria, not commands, so it cannot drift from what
is written here or in `.github/workflows/release.yml`.

## Homebrew

The tap is [`curvelogic/homebrew-tap`](https://github.com/curvelogic/homebrew-tap).
`acetone` ships as a **binary formula**: per-platform `url` + `sha256` pointing
at the published release archives (the `eucalypt.rb` formula in that tap is the
style to match). The archives contain the `acetone` binary at their root, so the
formula's install is a plain `bin.install "acetone"`.

The bump is automated: publishing a release triggers
`.github/workflows/homebrew-bump.yml`, which regenerates `Formula/acetone.rb`
from the release's four `.sha256` assets (via
`scripts/generate-homebrew-formula.sh`, runnable locally against downloaded
assets) and opens a PR on the tap for review. It can also be run manually from
the Actions tab with a tag input (e.g. to retry a failed bump).

Cross-repo write needs a dedicated secret, since the default `GITHUB_TOKEN`
cannot write to another repository: **`HOMEBREW_TAP_TOKEN`**, a repository
secret on `curvelogic/acetone` holding a fine-grained PAT restricted to
`curvelogic/homebrew-tap` with **Contents: read and write** and
**Pull requests: read and write**. Until it exists the workflow's final step
fails with instructions (everything before it runs read-only).

## The library crates (held)

**crates.io publication is on hold as standing policy** (Greg, 2026-07-21;
ADR-0047 point 5): the crates are not published until Greg judges the project
mature enough, or an external need forces it. Publication was originally
deferred past 0.1 (Greg, 2026-07-10) to keep the Phase-7 seam fixes free to
change; the `acetone-core` API has since **frozen at the 0.2 gate** (ADR-0046,
see `STABILITY.md`), so the hold — not API stability — is now what keeps the
crates internal.

If and when the library is published, do it **bottom-up** in dependency order so
each crate's dependencies are already on the index:

```
acetone-store  →  acetone-prolly  →  acetone-model  →  acetone-cypher
              →  acetone-graph   →  acetone-core   (→ acetone-cli last)
```

`acetone-bench`, `acetone-tck` and `acetone-lab` are internal and stay
`publish = false`. Each publishable crate already carries complete metadata
(name, version, `license`, `description`, `repository`) and versioned path
dependencies, so `cargo publish --dry-run -p acetone-store` verifies the leaf;
downstream crates only dry-run clean once their dependencies are on crates.io.
