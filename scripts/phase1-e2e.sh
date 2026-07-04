#!/usr/bin/env bash
# Phase 1 exit script (acetone-63m.8, roadmap Phase 1 exit criteria).
#
# Drives the acetone CLI end to end against a real repository: init,
# plumbing writes, commit, branch, mutate, root-level diff, fsck, native
# git interop and gc survival — and, when E2E_REMOTE is set to a git URL,
# a real push/clone round trip that must work against a remote that
# knows nothing about acetone (spec §3.5: "a remote need not know
# acetone exists").
#
# Every step asserts; any failure exits non-zero. CI runs everything
# except the remote round trip (the GitHub push step is evidenced
# manually per the bead's acceptance criteria).
#
# Usage:
#   cargo build --release -p acetone-cli
#   scripts/phase1-e2e.sh
#   E2E_REMOTE=git@github.com:owner/private-repo.git scripts/phase1-e2e.sh

set -euo pipefail

ACETONE="${ACETONE:-target/release/acetone}"
if [ ! -x "$ACETONE" ]; then
    echo "acetone binary not found at $ACETONE — build with:" >&2
    echo "  cargo build --release -p acetone-cli" >&2
    exit 2
fi
ACETONE="$(cd "$(dirname "$ACETONE")" && pwd)/$(basename "$ACETONE")"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
REPO="$WORK/graph.git"

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# Assertion style: `cmd | grep -q X || fail` composes correctly with
# pipefail (and outputs here are far below the pipe buffer, so grep -q
# closing early cannot SIGPIPE the writer). Do NOT assert with
# `cmd | grep X && fail || true` — a non-zero cmd skips the && branch
# and the failure is swallowed.

step "init"
"$ACETONE" init "$REPO"

step "plumbing writes"
"$ACETONE" --repo "$REPO" put-node Host web1 --prop os=linux --prop cores=8
"$ACETONE" --repo "$REPO" put-node Host web2 --prop os=bsd
"$ACETONE" --repo "$REPO" put-node Service db --prop tier=0
"$ACETONE" --repo "$REPO" put-edge Host web1 DEPENDS_ON Service db
"$ACETONE" --repo "$REPO" status | grep -q "nodes: 3, edges: 1" \
    || fail "status must count 3 nodes, 1 edge"

step "commit with trailer"
"$ACETONE" --repo "$REPO" commit -m "initial infrastructure" \
    --trailer Acetone-Source=phase1-e2e
"$ACETONE" --repo "$REPO" log | grep -q "initial infrastructure" \
    || fail "log must show the commit"
"$ACETONE" --repo "$REPO" log | grep -q "Acetone-Source: phase1-e2e" \
    || fail "log must show the trailer"

step "branch, mutate, commit"
"$ACETONE" --repo "$REPO" branch feature
"$ACETONE" --repo "$REPO" checkout feature
"$ACETONE" --repo "$REPO" put-node Host web3 --prop os=linux
"$ACETONE" --repo "$REPO" commit -m "add web3"

step "root-level diff: diverged branches have different manifest roots"
MAIN_MANIFEST=$(git -C "$REPO" rev-parse main:manifest)
FEATURE_MANIFEST=$(git -C "$REPO" rev-parse feature:manifest)
[ "$MAIN_MANIFEST" != "$FEATURE_MANIFEST" ] \
    || fail "diverged branches must differ at the manifest root"

step "checkout back: workspace returns to main's version"
"$ACETONE" --repo "$REPO" checkout main
"$ACETONE" --repo "$REPO" list-nodes --label Host | grep -c '"Host"' | grep -qx 2 \
    || fail "main must have exactly 2 hosts"

step "fsck (acetone)"
"$ACETONE" --repo "$REPO" fsck | grep -q "fsck: clean" || fail "fsck must be clean"

step "native git interop"
git -C "$REPO" log --oneline main | grep -q "initial infrastructure" \
    || fail "git log must render the acetone commit"
# git fsck exits non-zero on real damage; dangling objects (superseded
# workspace manifests, spec §4 garbage) are warnings and exit 0. Assert
# on the exit code — a grep over its output composes badly with pipefail
# (a non-zero fsck would skip the && branch and be swallowed).
FSCK_OUT="$(git -C "$REPO" fsck --strict 2>&1)" \
    || fail "git fsck reported problems: $FSCK_OUT"

step "gc survival: committed versions survive git gc --prune=now"
git -C "$REPO" gc --prune=now --quiet
"$ACETONE" --repo "$REPO" get-node Host web1 | grep -q '"os": "linux"' \
    || fail "node data must survive gc"
"$ACETONE" --repo "$REPO" fsck | grep -q "fsck: clean" || fail "fsck must be clean after gc"

if [ -n "${E2E_REMOTE:-}" ]; then
    step "remote round trip: push"
    git -C "$REPO" push --quiet "$E2E_REMOTE" main feature

    step "remote round trip: clone back and verify"
    CLONE="$WORK/clone.git"
    git clone --quiet --bare "$E2E_REMOTE" "$CLONE"
    FSCK_OUT="$(git -C "$CLONE" fsck --strict 2>&1)" \
        || fail "cloned repository must be connected: $FSCK_OUT"
    # Workspace refs are local-only (never pushed); recreate the default
    # workspace in the clone by pointing it at main's manifest blob —
    # pure git plumbing, then acetone opens the clone like any repo.
    git -C "$CLONE" update-ref refs/acetone/workspaces/default \
        "$(git -C "$CLONE" rev-parse main:manifest)"
    git -C "$CLONE" symbolic-ref HEAD refs/heads/main
    "$ACETONE" --repo "$CLONE" get-node Host web1 | grep -q '"os": "linux"' \
        || fail "clone must serve node data"
    "$ACETONE" --repo "$CLONE" log | grep -q "initial infrastructure" \
        || fail "clone must serve history"
    "$ACETONE" --repo "$CLONE" fsck | grep -q "fsck: clean" \
        || fail "clone must fsck clean"
else
    step "remote round trip SKIPPED (set E2E_REMOTE to a private git URL to run)"
fi

echo
echo "PHASE 1 E2E: OK"
