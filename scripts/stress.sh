#!/usr/bin/env bash
# Cross-process concurrency check: the invariants that unit tests prove on one
# connection must also hold when ten independent CLI processes race.
#
#   1. N concurrent claims on one item  -> exactly one winner, rest claim_conflict
#   2. N concurrent writes at version V -> exactly one success, rest version_conflict
#
# Usage: bash scripts/stress.sh [N] [apm-binary]
set -euo pipefail

N="${1:-10}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APM="${2:-${APM_BIN:-$ROOT/target/debug/apm}}"

if [[ ! -x "$APM" ]]; then
  echo "building apm…"
  (cd "$ROOT" && cargo build -q)
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export APM_DB="$WORK/.apm/apm.db"

jq_get() { python3 -c "import json,sys;print(json.load(sys.stdin)$1)"; }

"$APM" init --name Stress --prefix STR --json >/dev/null
ID="$("$APM" create --title "Race target" --goal "prove mutual exclusion" \
      --current-state "fresh" --json | jq_get "['item']['identifier']")"

echo "item: $ID   contenders: $N"

# --- 1. concurrent claims -----------------------------------------------------
for i in $(seq 1 "$N"); do
  (
    APM_AGENT="agent-$i" APM_SESSION_ID="sess-$i" \
      "$APM" claim "$ID" --json >"$WORK/claim.$i.json" 2>&1
    echo $? >"$WORK/claim.$i.code"
  ) &
done
wait

wins=$(grep -l '"ok": true' "$WORK"/claim.*.json | wc -l | tr -d ' ')
conflicts=$(grep -l 'claim_conflict' "$WORK"/claim.*.json | wc -l | tr -d ' ')
echo "claims: $wins winner(s), $conflicts conflict(s)"

# --- 2. concurrent writes at the same expected version ------------------------
# The winner's session owns the token; use it for the write race.
WINNER_FILE=$(grep -l '"ok": true' "$WORK"/claim.*.json | head -1)
WINNER_SESSION=$(python3 -c "import json;print(json.load(open('$WINNER_FILE'))['claim']['session_id'])")
WINNER_AGENT=$(python3 -c "import json;print(json.load(open('$WINNER_FILE'))['claim']['holder'])")

for i in $(seq 1 "$N"); do
  (
    APM_AGENT="$WINNER_AGENT" APM_SESSION_ID="$WINNER_SESSION" \
      "$APM" progress "$ID" --expect-version 1 --summary "writer $i" --json \
      >"$WORK/write.$i.json" 2>&1
    echo $? >"$WORK/write.$i.code"
  ) &
done
wait

ok=$(grep -l '"ok": true' "$WORK"/write.*.json | wc -l | tr -d ' ')
stale=$(grep -l 'version_conflict' "$WORK"/write.*.json | wc -l | tr -d ' ')
echo "writes: $ok success(es), $stale version_conflict(s)"

status=$("$APM" get "$ID" --json)
version=$(printf '%s' "$status" | jq_get "['item']['version']")
events=$(printf '%s' "$status" | jq_get "['history_len']")
echo "final: version=$version, events=$events"

fail=0
[[ "$wins" == "1" ]] || { echo "FAIL: expected exactly one claim winner"; fail=1; }
[[ "$conflicts" == "$((N - 1))" ]] || { echo "FAIL: expected $((N - 1)) claim conflicts"; fail=1; }
[[ "$ok" == "1" ]] || { echo "FAIL: expected exactly one successful write"; fail=1; }
[[ "$stale" == "$((N - 1))" ]] || { echo "FAIL: expected $((N - 1)) version conflicts"; fail=1; }
[[ "$version" == "2" ]] || { echo "FAIL: expected version 2 after one write"; fail=1; }
[[ "$(printf '%s' "$status" | jq_get "['item']['progress_summary']")" == "writer "* ]] \
  || { echo "FAIL: progress summary was not persisted"; fail=1; }

if [[ "$fail" == "0" ]]; then
  echo "PASS"
else
  echo "FAILED"
  exit 1
fi
