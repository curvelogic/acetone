#!/usr/bin/env bash
# Re-generate the committed public-API snapshots that enforce the 0.2 API
# freeze (ADR-0046). Run this after an INTENTIONAL change to the public surface,
# review the diff, and commit it alongside the change.
#
# The snapshots are pinned to a specific rustdoc-JSON format: cargo-public-api
# reads rustdoc's unstable JSON, so this must run under the same nightly the CI
# `public-api` job pins. Bump the nightly, the tool version, and the snapshots
# together (see ADR-0046 and STABILITY.md).
set -euo pipefail

NIGHTLY="${ACETONE_PUBLIC_API_TOOLCHAIN:-nightly-2026-07-18}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if ! command -v cargo-public-api >/dev/null 2>&1; then
  echo "cargo-public-api not found. Install the pinned version:" >&2
  echo "  cargo install cargo-public-api --locked --version 0.52.0" >&2
  exit 1
fi

echo "Blessing public-API snapshots with ${NIGHTLY}…"
for crate in acetone-core acetone-cypher acetone-graph acetone-model acetone-prolly acetone-store; do
  RUSTUP_TOOLCHAIN="${NIGHTLY}" cargo public-api --package "${crate}" \
    > "${ROOT}/crates/${crate}/public-api.txt"
done
echo "Done. Review the diff and commit:"
echo "  git diff -- crates/*/public-api.txt"
