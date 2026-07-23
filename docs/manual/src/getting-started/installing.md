# Installing acetone

acetone ships as a **single binary**. There is no server to run, no daemon to
configure and no database service to keep alive: you install the `acetone`
command, and every graph you work with is an ordinary directory on disk.

There are three ways to get it.

## Homebrew (macOS and Linux)

The [`curvelogic/tap`](https://github.com/curvelogic/homebrew-tap) tap carries
a binary formula for every supported platform (Apple Silicon and Intel macOS,
x86-64 and ARM Linux):

```sh
brew install curvelogic/tap/acetone
```

## Release binaries

Every release on the
[GitHub Releases page](https://github.com/curvelogic/acetone/releases)
attaches a `.tar.gz` per target, each with a matching `.sha256` checksum file:

| Archive suffix | Platform |
|---|---|
| `x86_64-unknown-linux-musl` | Linux, x86-64 — statically linked, no libc dependency |
| `aarch64-unknown-linux-musl` | Linux, ARM64 — statically linked, no libc dependency |
| `aarch64-apple-darwin` | macOS, Apple Silicon |
| `x86_64-apple-darwin` | macOS, Intel |

The Linux binaries are fully static (musl), so they run on any distribution
without further dependencies. Each archive contains the `acetone` binary at
its root. For example, on an Apple Silicon Mac (substitute the current version
and your target):

```sh
curl -LO https://github.com/curvelogic/acetone/releases/download/v0.3.0/acetone-v0.3.0-aarch64-apple-darwin.tar.gz
curl -LO https://github.com/curvelogic/acetone/releases/download/v0.3.0/acetone-v0.3.0-aarch64-apple-darwin.tar.gz.sha256
shasum -a 256 -c acetone-v0.3.0-aarch64-apple-darwin.tar.gz.sha256
tar xzf acetone-v0.3.0-aarch64-apple-darwin.tar.gz
install -m 755 acetone ~/.local/bin/    # or anywhere on your PATH
```

## Building from source

acetone is a standard Cargo workspace; a release build needs nothing beyond a
Rust toolchain (Rust 1.96 or later):

```sh
git clone https://github.com/curvelogic/acetone
cd acetone
cargo build --release --bin acetone     # binary at target/release/acetone
```

The library crates are not yet published to crates.io — building from source
means building the repository. If you want acetone as a Rust *library* rather
than a CLI, see [the library API](../reference/library-api.md).

## Verify the installation

```console
$ acetone --version
acetone 0.3.0
```

`acetone --help` prints every command, grouped the way you will meet them in
this manual — everyday version control (`init`, `status`, `commit`, `log`,
`branch`, `checkout`, `diff`, `merge`, `resolve`), schema (`declare-label`,
`declare-rel-type`, `declare-index`, `reindex`, `schema`), data and query
(`import`, `export`, `query`, `shell`), maintenance (`fsck`, `gc`, `migrate`)
and plumbing (`put-node`, `get-node`, `put-edge`, `list-nodes`, `rekey`).

## The `--repo` convention

Every command takes `--repo <path>`, naming the repository to operate on — or
**any subdirectory of it**: like `git -C`, acetone discovers the enclosing
repository by walking up parent directories. The default is the current
directory, so in practice you `cd` into a repository and type plain commands,
exactly as you would with git:

```sh
acetone status                  # the repository enclosing the current directory
acetone --repo ~/graphs/assets status   # a repository somewhere else
```

The one exception is `init`, which takes an optional `PATH` argument for the
directory to create the repository in and ignores `--repo` when given one.

## One thing to know before you start

An acetone repository *is* a git repository. Its history is a real git commit
graph, and backup and transport are plain git: `git clone`, `git push` and
`git pull` on the enclosing repository, against any git remote — the remote
need not know acetone exists. That is why acetone has no push/pull/clone
subcommands of its own. What git must **not** be used for is writing:
`commit`, `merge`, `checkout` and friends go through acetone, which knows how
to write commits acetone can read. `acetone --help` ends with a summary of
this division of labour.

Next: [create your first graph](first-graph.md).
