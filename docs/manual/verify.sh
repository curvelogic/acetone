#!/usr/bin/env bash
# Verify the manual's load-bearing runnable artefacts against a real acetone
# binary (acetone-4zy.8). This is NOT transcript diffing — commit hashes and
# timestamps legitimately differ between runs — it asserts OUTCOMES: counts,
# exit codes and key output strings that the manual presents as facts.
#
# Covered (see the PR/bead for the boundary rationale):
#   A. getting-started/asset-registry.{md,sh} — the committed build script,
#      end to end: seed counts, schema entries, constraint error, one query.
#   B. working/importing.md — the committed data files through the documented
#      import commands: counts, provenance (incl. the chapter's exact source
#      hash), no-op re-import, error strings, the mirror-branch curation
#      merge, and the JSON export round trip.
#   C. reference/library-api.md — the compiled cargo example's documented
#      output (compilation itself is cargo's job).
#
# Usage:
#   ACETONE=path/to/acetone EXAMPLE_BIN=path/to/manual_library_api \
#     docs/manual/verify.sh
set -euo pipefail

MANUAL_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="$MANUAL_DIR/src"

ACETONE="${ACETONE:?set ACETONE to the acetone binary to verify against}"
ACETONE="$(cd "$(dirname "$ACETONE")" && pwd)/$(basename "$ACETONE")"
EXAMPLE_BIN="${EXAMPLE_BIN:?set EXAMPLE_BIN to the built manual_library_api example}"
EXAMPLE_BIN="$(cd "$(dirname "$EXAMPLE_BIN")" && pwd)/$(basename "$EXAMPLE_BIN")"

# asset-registry.sh invokes plain `acetone`; give it the binary under test.
PATH="$(dirname "$ACETONE"):$PATH"
export PATH

# Commits need an identity; default one so the script runs anywhere (CI included).
export GIT_AUTHOR_NAME="${GIT_AUTHOR_NAME:-manual-verify}"
export GIT_AUTHOR_EMAIL="${GIT_AUTHOR_EMAIL:-manual-verify@acetone.invalid}"
export GIT_COMMITTER_NAME="${GIT_COMMITTER_NAME:-manual-verify}"
export GIT_COMMITTER_EMAIL="${GIT_COMMITTER_EMAIL:-manual-verify@acetone.invalid}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

CHECKS=0

fail() {
    echo "FAIL: $1" >&2
    if [ -n "${2:-}" ]; then
        echo "--- output ---" >&2
        printf '%s\n' "$2" >&2
        echo "--------------" >&2
    fi
    exit 1
}

ok() {
    CHECKS=$((CHECKS + 1))
    echo "  ok: $1"
}

# assert_contains HAYSTACK NEEDLE LABEL — quoted pattern, so needles may
# contain [, ], ( etc. without glob interpretation.
assert_contains() {
    case "$1" in
        *"$2"*) ok "$3" ;;
        *) fail "$3 — expected output to contain: $2" "$1" ;;
    esac
}

# expect_error CMD... — runs the command, requiring a non-zero exit; the
# combined output lands in $ERR_OUT for a follow-up assert_contains.
expect_error() {
    if ERR_OUT="$("$@" 2>&1)"; then
        fail "expected failure but command succeeded: $*" "$ERR_OUT"
    fi
}

head_hash() {
    "$ACETONE" status | sed -n 's/^HEAD: //p'
}

# --- A. The asset registry (getting-started/asset-registry.md + .sh) --------

echo "== A: asset-registry.sh end to end"

mkdir -p "$WORK/getting-started/registry"
cd "$WORK/getting-started/registry"
if ! BUILD_LOG="$(sh "$SRC/getting-started/asset-registry.sh" 2>&1)"; then
    fail "asset-registry.sh did not run cleanly" "$BUILD_LOG"
fi
ok "asset-registry.sh ran cleanly"

STATUS="$("$ACETONE" status)"
assert_contains "$STATUS" 'nodes: 12, edges: 15, schema entries: 7' \
    "seed status counts (12 nodes, 15 edges, 7 schema entries)"

SCHEMA="$("$ACETONE" schema)"
for entry in '"Host"' '"Service"' '"Team"' '"DEPENDS_ON"' '"OWNS"' '"RUNS_ON"' \
    'required ("tier")' '"host_by_region"'; do
    assert_contains "$SCHEMA" "$entry" "schema lists $entry"
done

# The documented existence-constraint rejection.
expect_error "$ACETONE" query 'CREATE (:Service {name: "search"})'
assert_contains "$ERR_OUT" 'missing required property "tier"' \
    "CREATE without required tier is rejected"

# The blast-radius query: every service is affected, owners resolved.
OUT="$("$ACETONE" query 'MATCH (h:Host {name: "db1"})<-[:RUNS_ON]-(s:Service)<-[:DEPENDS_ON*0..]-(affected:Service)<-[:OWNS]-(t:Team) RETURN DISTINCT affected.name, t.name, t.oncall ORDER BY affected.name')"
assert_contains "$OUT" '4 rows' "blast-radius query returns 4 rows"
assert_contains "$OUT" 'storefront' "blast radius reaches storefront"
assert_contains "$OUT" '#payments-oncall' "blast radius resolves on-call channels"

# --- B. Importing data end to end (working/importing.md) --------------------

echo "== B: importing.md against the committed data files"

mkdir -p "$WORK/importing/registry"
cp "$SRC/working/hosts.csv" "$SRC/working/runs_on.csv" \
    "$SRC/working/services.json" "$SRC/working/hosts-updated.csv" \
    "$WORK/importing/"
cd "$WORK/importing/registry"
if ! BUILD_LOG="$(sh "$SRC/getting-started/asset-registry.sh" 2>&1)"; then
    fail "asset-registry.sh (fresh copy for importing.md) did not run cleanly" "$BUILD_LOG"
fi
SEED="$(head_hash)"

# Nodes from CSV: 7 rows processed, only app3/db3 actually new.
OUT="$("$ACETONE" import --format csv ../hosts.csv --label Host)"
assert_contains "$OUT" 'imported 7 node(s) and 0 edge(s)' "hosts.csv imports 7 node rows"

LOG="$("$ACETONE" log)"
assert_contains "$LOG" 'Acetone-Source: ../hosts.csv' "provenance: source trailer"
assert_contains "$LOG" 'Acetone-Extractor: csv' "provenance: extractor trailer"
assert_contains "$LOG" \
    'Acetone-Source-Hash: f961337fe6981739e07185c4d11473688ca4e72df0126105cff5cf0aebe9afb2' \
    "provenance: the chapter's exact sha256 of hosts.csv"

DIFF="$("$ACETONE" diff "$SEED" main)"
assert_contains "$DIFF" '+ node "Host" ["app3"]' "diff shows app3 added"
assert_contains "$DIFF" '+ node "Host" ["db3"]' "diff shows db3 added"

# Re-importing an unchanged source is a no-op.
OUT="$("$ACETONE" import --format csv ../hosts.csv --label Host)"
assert_contains "$OUT" 'source unchanged; nothing imported' "unchanged re-import is a no-op"

# Relationships from CSV.
OUT="$("$ACETONE" import --format csv ../runs_on.csv --edge RUNS_ON \
    --from Service=service --to Host=host \
    -m "placement: the new hosts take billing and postgres")"
assert_contains "$OUT" 'imported 0 node(s) and 2 edge(s)' "runs_on.csv imports 2 edges"

# Typed values from JSON: ports arrive as integers.
OUT="$("$ACETONE" import --format json ../services.json --label Service)"
assert_contains "$OUT" 'imported 4 node(s) and 0 edge(s)' "services.json imports 4 node rows"
OUT="$("$ACETONE" query 'MATCH (s:Service) WHERE s.port < 1024 RETURN s.name, s.port')"
assert_contains "$OUT" 'storefront' "numeric port comparison finds storefront"
assert_contains "$OUT" '1 row' "numeric port comparison finds exactly one service"

assert_contains "$("$ACETONE" status)" 'nodes: 14, edges: 17, schema entries: 7' \
    "post-import status counts (14 nodes, 17 edges)"

# When the source is wrong: an undeclared label fails before touching the graph.
printf 'name,site\nr1,dc-lux\n' > ../racks.csv
expect_error "$ACETONE" import --format csv ../racks.csv --label Rack
assert_contains "$ERR_OUT" 'no schema for label "Rack"' "undeclared label is refused"

# ... and a dangling relationship is refused.
printf 'service,host\nbilling,ghost9\n' > ../runs_on-ghost.csv
expect_error "$ACETONE" import --format csv ../runs_on-ghost.csv --edge RUNS_ON \
    --from Service=service --to Host=host
assert_contains "$ERR_OUT" 'dangling RUNS_ON relationship' "dangling edge is refused"

# ... and a row violating a declared constraint fails the whole import
# (acetone-9gw: the chapter documents enforcement, not a gap).
printf '[{"name": "search", "version": "0.9.1", "port": 9200}]\n' > ../services-notier.json
expect_error "$ACETONE" import --format json ../services-notier.json --label Service
assert_contains "$ERR_OUT" 'import violates declared constraints' \
    "constraint-violating import is refused"
assert_contains "$ERR_OUT" 'missing required property "tier"' \
    "the violation names the missing property"

assert_contains "$("$ACETONE" status)" 'nodes: 14, edges: 17' \
    "failed imports left the graph untouched"

# Import as curation: the mirror branch. First --branch run creates the branch.
OUT="$("$ACETONE" import --format csv ../hosts.csv --label Host --branch ingest)"
assert_contains "$OUT" 'source unchanged; nothing imported' "mirror bootstrap is a no-op"
assert_contains "$("$ACETONE" branch)" 'ingest' "mirror branch exists"

# Curate on main.
"$ACETONE" query 'MATCH (h:Host {name: "app3"}) SET h.note = "canary: new capacity, watch error rates"' > /dev/null
"$ACETONE" commit -m "annotate app3 as the canary host" > /dev/null

# The next export lands on the mirror; main does not move.
OUT="$("$ACETONE" import --format csv ../hosts-updated.csv --label Host --branch ingest)"
assert_contains "$OUT" 'imported 8 node(s) and 0 edge(s) onto ingest' \
    "hosts-updated.csv lands on the mirror branch"
assert_contains "$("$ACETONE" status)" 'On branch main' "still on main after --branch import"
assert_contains "$("$ACETONE" status)" 'nodes: 14, edges: 17' "main unmoved by mirror import"

# Merge: curation survives, the source's updates land.
OUT="$("$ACETONE" merge ingest -m "take the July host inventory")"
assert_contains "$OUT" 'merge commit' "mirror merge is clean"
OUT="$("$ACETONE" query 'MATCH (h:Host {name: "app3"}) RETURN h.region, h.note')"
assert_contains "$OUT" 'canary: new capacity, watch error rates' \
    "curated note survived the re-import merge"
assert_contains "$OUT" 'eu-west' "app3 region intact after merge"
OUT="$("$ACETONE" query 'MATCH (h:Host) RETURN h.name ORDER BY h.name')"
assert_contains "$OUT" 'edge2' "new host edge2 arrived via the mirror"
assert_contains "$OUT" '8 rows' "host count after merge is 8"
OUT="$("$ACETONE" query 'MATCH (h:Host {name: "app2"}) RETURN h.region')"
assert_contains "$OUT" 'eu-west' "app2 rebuild (region change) landed from the source"

# Round trips: JSON export re-imports as a no-op.
"$ACETONE" export --format json --label Service -o ../service-export.json > /dev/null
OUT="$("$ACETONE" import --format json ../service-export.json --label Service)"
assert_contains "$OUT" 'source unchanged; nothing imported' "JSON round trip is faithful"

# --- C. The library API example (reference/library-api.md) ------------------

echo "== C: manual_library_api example output"

OUT="$("$EXAMPLE_BIN")"
assert_contains "$OUT" 'committed ' "example commits"
assert_contains "$OUT" '[String("web1"), Int(8)]' "example reads the documented row back"

echo "verify-manual: all $CHECKS checks passed"
