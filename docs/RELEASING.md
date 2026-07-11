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
   and the `acetone-*` path-dependency pins in `[workspace.dependencies]`, on
   `main`. `acetone --version` and the binaries then report it. (Already `0.1.0`
   for the first release.)
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
5. **Homebrew** follows publication (see below).

Build a binary locally with `cargo build --release --bin acetone`.

## Homebrew

The tap is [`curvelogic/homebrew-tap`](https://github.com/curvelogic/homebrew-tap).
`acetone` ships as a **binary formula**: per-platform `url` + `sha256` pointing
at the published release archives (the `eucalypt.rb` formula in that tap is the
style to match). The archives contain the `acetone` binary at their root, so the
formula's install is a plain `bin.install "acetone"`.

For the first release this is done by hand: after publishing, take each
archive's `sha256` (the `.sha256` asset, or `shasum -a 256`) and open a PR to the
tap. Automating it as a `release: published` workflow that opens the tap PR is a
follow-up (`acetone-wpx`); it needs a cross-repo token secret
(`HOMEBREW_TAP_TOKEN`) since the default `GITHUB_TOKEN` cannot write to another
repository.

## The library crates (deferred)

**crates.io publication is not part of 0.1** (Greg, 2026-07-10). The crates stay
clean and buildable but internal — no external API is frozen, which is precisely
why the Phase-7 seam fixes (rel identity, value domain, the library query API)
remain free to change. `acetone-core` is the intended library surface but it
stabilises at **0.2**, gated on the query-engine resource governor (spec §7).

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
