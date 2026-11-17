#!/usr/bin/env bash
# Cross-process REST concurrency check.
#
# Complements scripts/stress.sh (CLI-only): this one starts a real `apm-serve`
# and races N real HTTP clients, then mixes HTTP and CLI writers on one item.
#
#   1. N concurrent claims over HTTP      -> exactly one winner
#   2. N concurrent writes at version V   -> exactly one success
#   3. HTTP + CLI racing at version V     -> exactly one success, no lost update
#   4. N servers booting one token file   -> one token for everybody
#
# Usage: bash scripts/stress-rest.sh [N]
set -euo pipefail

N="${1:-10}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APM="${APM:-$ROOT/target/debug/apm}"
SERVE="${APM_SERVE:-$ROOT/target/debug/apm-serve}"

if [[ ! -x "$APM" || ! -x "$SERVE" ]]; then
  echo "building binaries…"
  (cd "$ROOT" && cargo build -q)
fi

WORK="$(mktemp -d)"
PIDS=()
cleanup() {
  # Kill and reap every server this script started: `wait` is what prevents
  # orphans when the script exits (or fails) between start-up and the end.
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in "${PIDS[@]:-}"; do
    wait "$pid" 2>/dev/null || true
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "work: $WORK"
export APM_DB="$WORK/.apm/apm.db"
"$APM" init --name "REST Stress" --prefix RSP --json >/dev/null
ID="$("$APM" create --title "REST race target" --goal "prove HTTP mutual exclusion" \
      --current-state "fresh" --json \
      | python3 -c 'import json,sys; print(json.load(sys.stdin)["item"]["identifier"])')"
echo "item: $ID   contenders: $N"

# --- 4. token bootstrap race: N servers, one token file ----------------------
TOKEN_FILE="$WORK/rest-token"
ADDRS=()
for _ in $(seq 1 "$N"); do
  # No extra subshell: it would swallow `$!`, leaving PIDS empty and the
  # servers running after this script exits.
  "$SERVE" --db "$APM_DB" --listen 127.0.0.1:0 --token-file "$TOKEN_FILE" \
    --print-token >"$WORK/serve.$RANDOM.log" 2>&1 &
  PIDS+=($!)
done
sleep 1.5
for log in "$WORK"/serve.*.log; do
  addr="$(sed -n 's/^listening on //p' "$log" | head -1)"
  [[ -n "$addr" ]] && ADDRS+=("$addr")
done
TOKEN="$(tr -d '[:space:]' <"$TOKEN_FILE")"
echo "servers up: ${#ADDRS[@]}   token length: ${#TOKEN}"
if [[ "${#ADDRS[@]}" -ne "$N" ]]; then
  echo "FAIL: expected $N servers, got ${#ADDRS[@]}"
  for log in "$WORK"/serve.*.log; do echo "--- $log"; cat "$log"; done
  exit 1
fi
# Every server must accept the token that is on disk.
for addr in "${ADDRS[@]}"; do
  code="$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "http://$addr/dashboard")"
  [[ "$code" == "200" ]] || { echo "FAIL: $addr rejected the file token ($code)"; exit 1; }
done
echo "token file: one token accepted by all ${#ADDRS[@]} servers"

ADDR="${ADDRS[0]}"

# --- 1. concurrent claims ----------------------------------------------------
seq 1 "$N" | xargs -P "$N" -I{} sh -c \
  "curl -sS -X POST -H 'Authorization: Bearer $TOKEN' -H 'X-APM-Session: sess-{}' \
     -d '{\"lease_seconds\":600}' http://$ADDR/items/$ID/claim > $WORK/claim.{}.json"

wins=$(grep -l '"token_suppressed": true' "$WORK"/claim.*.json | wc -l | tr -d ' ')
conflicts=$(grep -l 'claim_conflict' "$WORK"/claim.*.json | wc -l | tr -d ' ')
echo "claims: $wins winner(s), $conflicts conflict(s)"

# Use the winner's session for the write race.
WINNER_SESSION="$(python3 - "$WORK" <<'PY'
import glob, json, sys
for path in glob.glob(sys.argv[1] + "/claim.*.json"):
    data = json.load(open(path))
    if data.get("ok"):
        print(data["claim"]["session_id"])
        break
PY
)"
if [[ -z "$WINNER_SESSION" ]]; then echo "FAIL: no claim winner"; exit 1; fi
VERSION="$("$APM" get "$ID" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["item"]["version"])')"

# --- 2 + 3. HTTP writers and CLI writers at the same version -----------------
# Collect the writer PIDs and wait for them explicitly: a bare `wait` would also
# wait for the servers started above, which never exit on their own.
half=$(( (N + 1) / 2 ))
WRITERS=()
for i in $(seq 1 "$half"); do
  (
    curl -sS -X POST -H "Authorization: Bearer $TOKEN" -H "X-APM-Session: $WINNER_SESSION" \
      -d "{\"summary\":\"rest writer $i\",\"expect_version\":$VERSION}" \
      "http://$ADDR/items/$ID/progress" >"$WORK/write.rest.$i.json"
  ) &
  WRITERS+=($!)
done
for i in $(seq 1 "$((N - half))"); do
  (
    "$APM" progress "$ID" --session "$WINNER_SESSION" --expect-version "$VERSION" \
      --summary "cli writer $i" --json >"$WORK/write.cli.$i.json" 2>&1
  ) &
  WRITERS+=($!)
done
for pid in "${WRITERS[@]}"; do wait "$pid" 2>/dev/null || true; done

ok=$(grep -l '"ok": true' "$WORK"/write.*.json | wc -l | tr -d ' ')
stale=$(grep -l 'version_conflict' "$WORK"/write.*.json | wc -l | tr -d ' ')
echo "writes: $ok success(es), $stale version_conflict(s)"

FINAL="$("$APM" get "$ID" --json)"
final_version="$(printf '%s' "$FINAL" | python3 -c 'import json,sys; print(json.load(sys.stdin)["item"]["version"])')"
summary="$(printf '%s' "$FINAL" | python3 -c 'import json,sys; print(json.load(sys.stdin)["item"]["progress_summary"])')"
echo "final: version=$final_version summary=$summary"

fail=0
[[ "$wins" == "1" ]] || { echo "FAIL: expected exactly one claim winner"; fail=1; }
[[ "$conflicts" == "$((N - 1))" ]] || { echo "FAIL: expected $((N - 1)) claim conflicts"; fail=1; }
[[ "$ok" == "1" ]] || { echo "FAIL: expected exactly one successful write"; fail=1; }
[[ "$stale" == "$((N - 1))" ]] || { echo "FAIL: expected $((N - 1)) version conflicts"; fail=1; }
[[ "$final_version" == "$((VERSION + 1))" ]] || { echo "FAIL: expected exactly one version bump"; fail=1; }
[[ "$summary" == rest\ writer\ * || "$summary" == cli\ writer\ * ]] \
  || { echo "FAIL: persisted summary does not belong to a single winner"; fail=1; }

if [[ "$fail" == "0" ]]; then echo "PASS"; else echo "FAILED"; exit 1; fi
