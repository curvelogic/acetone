#!/usr/bin/env bash
# Phase 5 flagship demo (acetone-6g5.5): the scheduled-import simulation.
# Drives the real CLI: successive snapshots of a mutating source imported as
# commits, no-op detection on an unchanged snapshot, `diff` as the change
# report, and an index-accelerated query tracking the mutation.
#
# Usage: scripts/phase-5-demo.sh [workdir]   (default: a fresh mktemp dir)
set -euo pipefail

ACETONE="${ACETONE:-cargo run -q -p acetone-cli --bin acetone --}"
WORK="${1:-$(mktemp -d)}"
REPO="$WORK/registry"
SRC="$WORK/source"
mkdir -p "$SRC"

run() { echo "+ acetone $*"; $ACETONE --repo "$REPO" "$@"; echo; }

echo "== 1. Initialise the registry and declare its schema =="
$ACETONE init "$REPO"
run declare-label Host --key name
run declare-index host_os --label Host --property os
run commit -m "schema: Host(name) + index on os"

echo "== 2. Snapshot 1 — first sync of the source =="
cat > "$SRC/snap1.ndjson" <<'JSON'
{"name":"web1","os":"linux"}
{"name":"db1","os":"linux"}
{"name":"cache1","os":"linux"}
JSON
run import --format ndjson "$SRC/snap1.ndjson" --label Host

echo "== 3. Snapshot 2 — the source mutated (web1 re-imaged; a host added) =="
cat > "$SRC/snap2.ndjson" <<'JSON'
{"name":"web1","os":"windows"}
{"name":"db1","os":"linux"}
{"name":"cache1","os":"linux"}
{"name":"new1","os":"linux"}
JSON
run import --format ndjson "$SRC/snap2.ndjson" --label Host

echo "== 4. The change report between the two runs is 'diff' =="
run log
# Commit lines start with a 40-hex hash; trailer lines are indented.
HASHES=$($ACETONE --repo "$REPO" log | grep -E '^[0-9a-f]{40} ' | cut -d' ' -f1)
TO=$(echo "$HASHES" | sed -n '1p')    # snapshot-2 import
FROM=$(echo "$HASHES" | sed -n '2p')  # snapshot-1 import
run diff "$FROM" "$TO"

echo "== 5. Re-import an UNCHANGED snapshot — detected no-op, no commit =="
cp "$SRC/snap2.ndjson" "$SRC/snap3.ndjson"
run import --format ndjson "$SRC/snap3.ndjson" --label Host

echo "== 6. The index tracked the mutation: web1 is now found under os=windows =="
run query "MATCH (h:Host {os: 'windows'}) RETURN h.name"

echo "== 7. Integrity: fsck is clean; gc consolidates the churn =="
run fsck
run gc

echo "Demo complete. Repository at: $REPO"
