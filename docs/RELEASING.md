# Releasing acetone

Two independent artefacts ship at a release: the **single static binary**
(`acetone`) and the **library crates** (headed by `acetone-core`).

## The binary

The `Release` workflow (`.github/workflows/release.yml`) triggers on a `v*`
tag. It builds `--release` (the workspace `[profile.release]`: `strip`, `lto`,
one codegen unit) for each target and attaches a `.tar.gz` plus a `.sha256` to
the GitHub release:

- `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` — statically linked
  against musl, self-contained (no libc dependency).
- `aarch64-apple-darwin` — the platform binary (a fully static binary is not
  supported on macOS).

To cut a release: `git tag vX.Y.Z && git push origin vX.Y.Z`. Build the binary
locally with `cargo build --release --bin acetone`.

## The library crates

`acetone-core` is the product surface (spec §7); it is a façade over the
constituent crates, which must therefore be published too. crates.io requires
every dependency to already be on the index, so publish **bottom-up** in
dependency order:

```
acetone-store  →  acetone-prolly  →  acetone-model  →  acetone-cypher
              →  acetone-graph   →  acetone-core
```

(`acetone-cli` — the binary crate — may be published last for `cargo install
acetone-cli`.) `acetone-bench`, `acetone-tck` and `acetone-lab` are internal
and stay `publish = false`.

Each crate carries complete publish metadata (name, version, `license`,
`description`, `repository`), and path dependencies carry a `version` so
`cargo publish` can resolve them. Verify the leaf before publishing:

```sh
cargo publish --dry-run -p acetone-store
```

A downstream crate only dry-runs clean **after** its dependencies are on
crates.io (the workspace is unpublished pre-0.1, so a full-chain dry-run is not
possible locally). Publish for real, bottom-up, only when tagging 0.1 — it
reserves the crate names irreversibly:

```sh
for c in acetone-store acetone-prolly acetone-model acetone-cypher acetone-graph acetone-core; do
  cargo publish -p "$c"   # wait for each to appear on the index before the next
done
```
