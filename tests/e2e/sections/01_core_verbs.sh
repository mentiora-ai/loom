# shellcheck shell=bash
# tests/e2e/sections/01_core_verbs.sh — Sections 1–11: doctor, session create, and each core web.* verb.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

# -- Section 1: Doctor + create session ---------------------------------
sect "Section 1: doctor + session create"
DOCTOR=$($LOOM doctor 2>/dev/null)
# Only require daemon_responsive=ok. `chromium_present_and_verified` can
# legitimately fail on CI runners using a system Chrome fallback rather
# than the postinstalled pinned build (macOS layout mismatch tracked as
# a v0.9.x fix), and the daemon stays fully functional through that
# fallback. The actual chromium signal is web.navigate working below.
if echo "$DOCTOR" | jq -e '.checks[] | select(.name == "daemon_responsive") | .status == "ok"' >/dev/null 2>&1; then
  ok "doctor-daemon-responsive"
else
  fail "doctor-daemon-responsive" "$DOCTOR"
fi

SESSION=$($LOOM session create --profile standard 2>&1 | jq -r .session_id 2>/dev/null)
if [[ "$SESSION" =~ ^[a-z0-9]{26}$ ]]; then
  ok "session-create-returns-ulid"
else
  fail "session-create-returns-ulid" "got '$SESSION'"
  exit 1
fi
echo "  session: $SESSION"

# -- Section 2: web.navigate happy path ---------------------------------
sect "Section 2: web.navigate to local fixture"
NAV=$(nav "$SESSION" "$FIXTURE_URL")
if echo "$NAV" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "navigate-returns-action-hash"
  echo "  action_hash: $(echo "$NAV" | jq -r '.action_hash' | cut -c1-16)…"
else
  fail "navigate-returns-action-hash" "$NAV"
fi

# -- Section 3: web.evaluate --------------------------------------------
sect "Section 3: web.evaluate"
EVAL=$(ev "$SESSION" 'document.title')
TITLE=$(echo "$EVAL" | jq -r '.return_value_json // empty' | jq -r 'select(. != null)' 2>/dev/null)
if [ "$TITLE" = "Loom E2E Fixture" ]; then
  ok "evaluate-returns-document-title"
else
  fail "evaluate-returns-document-title" "got '$TITLE' from: $EVAL"
fi

RAND1=$(ev "$SESSION" 'Math.random()' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
RAND2=$(ev "$SESSION" 'Math.random()' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
if [ -n "$RAND1" ] && [ -n "$RAND2" ]; then
  ok "evaluate-math-random-returns-values"
else
  fail "evaluate-math-random-returns-values" "rand1='$RAND1' rand2='$RAND2'"
fi

# -- Section 4: web.wait ------------------------------------------------
sect "Section 4: web.wait"
W=$(wait_ "$SESSION" '#hello' 2000)
if echo "$W" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "wait-resolves-existing-selector"
else
  fail "wait-resolves-existing-selector" "$W"
fi

# -- Section 5: web.type ------------------------------------------------
sect "Section 5: web.type"
T=$(type_ "$SESSION" '#text-input' 'hello world')
if echo "$T" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "type-returns-receipt"
  VAL=$(ev "$SESSION" 'document.getElementById("text-input").value' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
  if [ "$VAL" = "hello world" ]; then
    ok "type-actually-set-value"
  else
    fail "type-actually-set-value" "got '$VAL'"
  fi
else
  fail "type-returns-receipt" "$T"
fi

# -- Section 6: web.click -----------------------------------------------
sect "Section 6: web.click"
C=$(click "$SESSION" '#ok-button')
if echo "$C" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "click-returns-receipt"
  RES=$(ev "$SESSION" 'document.getElementById("result").textContent' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
  if [ "$RES" = "clicked" ]; then ok "click-fired-handler"
  else fail "click-fired-handler" "got '$RES'"; fi
else
  fail "click-returns-receipt" "$C"
fi

# -- Section 7: web.select ----------------------------------------------
sect "Section 7: web.select"
S=$(sel "$SESSION" '#dropdown' 'b')
if echo "$S" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "select-returns-receipt"
  V=$(ev "$SESSION" 'document.getElementById("dropdown").value' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
  if [ "$V" = "b" ]; then ok "select-changed-value"
  else fail "select-changed-value" "got '$V'"; fi
else
  fail "select-returns-receipt" "$S"
fi

# -- Section 8: web.scroll ----------------------------------------------
sect "Section 8: web.scroll"
SC=$(scroll "$SESSION" 'body' 100)
if echo "$SC" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "scroll-returns-receipt"
else
  fail "scroll-returns-receipt" "$SC"
fi

# -- Section 9: web.hover -----------------------------------------------
sect "Section 9: web.hover"
H=$(hover "$SESSION" '#ok-button')
if echo "$H" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "hover-returns-receipt"
else
  fail "hover-returns-receipt" "$H"
fi

# -- Section 10: web.screenshot -----------------------------------------
sect "Section 10: web.screenshot"
SH=$(shot "$SESSION")
if echo "$SH" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "screenshot-returns-receipt"
  # Note: docs/actions.md says "screenshot_ref" but the actual wire field is
  # "screenshot_after_hash". Tracking that drift but accepting either here.
  REF=$(echo "$SH" | jq -r '.screenshot_ref // .screenshot_after_hash // empty')
  if [ -n "$REF" ] && [ "$REF" != "null" ]; then
    ok "screenshot-has-content-hash (${REF:0:16}…)"
  else
    fail "screenshot-has-content-hash" "no screenshot_ref or screenshot_after_hash"
  fi
else
  fail "screenshot-returns-receipt" "$SH"
fi

# -- Section 11: web.snapshot -------------------------------------------
sect "Section 11: web.snapshot"
SN=$(snap "$SESSION")
if echo "$SN" | jq -e '.action_hash' >/dev/null 2>&1; then
  ok "snapshot-returns-receipt"
else
  fail "snapshot-returns-receipt" "$SN"
fi
