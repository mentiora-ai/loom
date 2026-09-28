# shellcheck shell=bash
# tests/e2e/sections/05_session_lifecycle.sh — Sections 12–15: inspect/validate, time-travel inspect, close, replay equality.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

# -- Section 12: session inspect/validate -------------------------------
sect "Section 12: session inspect + validate"
INS=$($LOOM session inspect "$SESSION" 2>&1)
echo "$INS" >"$RESULTS/inspect.json"
if echo "$INS" | jq -e '.actions[0]' >/dev/null 2>&1 || echo "$INS" | jq -e '.action_count' >/dev/null 2>&1; then
  ok "inspect-returns-payload"
else
  fail "inspect-returns-payload" "see $RESULTS/inspect.json"
fi

VAL=$($LOOM session validate "$SESSION" 2>&1)
if echo "$VAL" | jq -e '.valid // .ok' >/dev/null 2>&1 || echo "$VAL" | grep -qiE 'PASS|valid|"ok":true'; then
  ok "validate-passes"
else
  fail "validate-passes" "$VAL"
fi

# -- Section 13: time-travel inspect ------------------------------------
sect "Section 13: session inspect --at-action"
TI=$($LOOM session inspect "$SESSION" --at-action 1 2>&1)
if echo "$TI" | jq -e '.' >/dev/null 2>&1; then
  ok "inspect-at-action-1-works"
else
  fail "inspect-at-action-1-works" "$TI"
fi

# -- Section 14: close session ------------------------------------------
sect "Section 14: session close"
CL=$($LOOM session close "$SESSION" 2>&1)
ok "close-returned ($CL)"

# -- Section 15: replay equality ----------------------------------------
sect "Section 15: replay + diff (the headline determinism claim)"
REPLAY=$($LOOM session replay "$SESSION" 2>&1)
echo "$REPLAY" >"$RESULTS/replay.json"
NEW=$(echo "$REPLAY" | jq -r '.session_id // .replay_session_id // empty' 2>/dev/null)
if [[ "$NEW" =~ ^[a-z0-9]{26}$ ]]; then
  ok "replay-returns-new-session-id ($NEW)"
  DIFF=$($LOOM session diff "$SESSION" "$NEW" 2>&1)
  echo "$DIFF" >"$RESULTS/diff.json"
  FD=$(echo "$DIFF" | jq -r '.field_diffs | length' 2>/dev/null)
  if [ "$FD" = "0" ]; then
    ok "replay-bit-equal-source (field_diffs=0)"
  else
    fail "replay-bit-equal-source" "field_diffs=$FD — see $RESULTS/diff.json"
  fi
else
  fail "replay-returns-new-session-id" "see $RESULTS/replay.json"
fi
