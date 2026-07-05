# Vendored openCypher TCK feature files

Upstream: https://github.com/opencypher/openCypher — `tck/features/**`
Pinned commit: `677cbafabb8c3c5eed458fd3b1ec0daec8d67d23` (vendored 2026-07-05)
Licence: Apache-2.0 (see `LICENSE.upstream`, `NOTICE.upstream`)

Vendored rather than fetched so CI needs no network and the conformance
target is immutable until deliberately bumped. To bump: clone upstream,
copy `tck/features/**` over this directory, update the pinned commit here,
and re-run the harness — new or changed step vocabulary fails loudly.

Do not hand-edit feature files; acetone-specific skips and expectations
live in the runner (`tck/src/`), never in the vendored corpus.
