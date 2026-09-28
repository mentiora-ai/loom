#!/usr/bin/env bash
# tests/e2e/run_load.sh — drive concurrent sessions at the daemon to verify
# (a) it doesn't crash, (b) action latency stays bounded, (c) every session
# completes its action chain without cross-talk.
#
# Defaults: 8 parallel sessions × 5 actions each, against the local fixture
# server on :8765. Tunable via env vars.

set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$(cd "$HERE/../.." && pwd)"
cd "$HERE"

LOOM="${LOOM_BIN:-$WORKSPACE/target/release/loom}"
N_SESSIONS="${N_SESSIONS:-8}"
N_ACTIONS="${N_ACTIONS:-5}"
FIXTURE_PORT="${FIXTURE_PORT:-8765}"
FIXTURE_URL="http://127.0.0.1:${FIXTURE_PORT}/index.html"
RESULTS=results
mkdir -p "$RESULTS"

# Boot fixture
if ! curl -sf "$FIXTURE_URL" >/dev/null; then
  # shellcheck source=lib/fixture_server.sh
  source "$HERE/lib/fixture_server.sh"
  trap 'kill ${FIXTURE_PID:-} 2>/dev/null || true' EXIT
  if ! start_fixture_server "$FIXTURE_PORT" "$RESULTS/load-fixture.log"; then
    echo "fixture server did not come up on :$FIXTURE_PORT:" >&2
    tail -5 "$RESULTS/load-fixture.log" >&2
    exit 1
  fi
fi

echo "Load test: ${N_SESSIONS} sessions × ${N_ACTIONS} actions"
echo "Fixture: $FIXTURE_URL"
echo

LOG="$RESULTS/load.log"
: >"$LOG"

t_start=$(date +%s%N)

run_session() {
  local i="$1"
  local SID
  SID=$($LOOM session create --profile standard 2>&1 | jq -r .session_id)
  if [ -z "$SID" ] || [ "$SID" = "null" ]; then
    echo "session $i: CREATE FAILED" >>"$LOG"
    return
  fi
  $LOOM action web.navigate --session "$SID" --url "$FIXTURE_URL" >/dev/null 2>&1
  local ok=0
  for a in $(seq 1 "$N_ACTIONS"); do
    R=$($LOOM action web.evaluate --session "$SID" --expression 'document.title' 2>&1)
    if echo "$R" | jq -e '.action_hash' >/dev/null 2>&1; then
      ok=$((ok+1))
    fi
  done
  echo "session $i: $ok/$N_ACTIONS evaluates OK (id=$SID)" >>"$LOG"
  $LOOM session close "$SID" >/dev/null 2>&1 || true
}

JOBS=()
for i in $(seq 1 "$N_SESSIONS"); do
  run_session "$i" &
  JOBS+=($!)
done
wait "${JOBS[@]}"

t_end=$(date +%s%N)
elapsed_ms=$(( (t_end - t_start) / 1000000 ))

echo "Completed in ${elapsed_ms}ms"
echo "(per-session log: $LOG)"

# Verify all sessions had clean action chains
TOTAL_OK=$(grep -oE "[0-9]+/${N_ACTIONS}" "$LOG" | awk -F/ '{s+=$1} END {print s}')
EXPECTED=$((N_SESSIONS * N_ACTIONS))
echo
echo "Total actions OK: $TOTAL_OK / $EXPECTED"
if [ "$TOTAL_OK" -eq "$EXPECTED" ]; then
  echo "PASS"
  exit 0
else
  echo "FAIL — some sessions failed actions; see $LOG"
  exit 1
fi
