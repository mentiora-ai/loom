# shellcheck shell=bash
# tests/e2e/sections/07_parallel_flows.sh — Sections 18–20: parallel sessions, the checkout flow, a real public site.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

# -- Section 18: parallel sessions --------------------------------------
sect "Section 18: parallel sessions (4 concurrent)"
PARALLEL_OUT="$RESULTS/parallel.log"
: >"$PARALLEL_OUT"
parallel_one() {
  SID=$($LOOM session create --profile standard 2>&1 | jq -r .session_id)
  nav "$SID" "$FIXTURE_URL" >/dev/null
  T=$(ev "$SID" 'document.title' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
  echo "session $1 title='$T' id=$SID" >>"$PARALLEL_OUT"
  $LOOM session close "$SID" >/dev/null 2>&1 || true
}
PJOBS=()
for i in 1 2 3 4; do parallel_one "$i" & PJOBS+=($!); done
wait "${PJOBS[@]}"
GOOD=$(grep -c "title='Loom E2E Fixture'" "$PARALLEL_OUT" || echo 0)
if [ "$GOOD" = "4" ]; then
  ok "four-parallel-sessions-all-loaded"
else
  fail "four-parallel-sessions-all-loaded" "$GOOD/4 — see $PARALLEL_OUT"
fi

# -- Section 19: full form/checkout flow --------------------------------
sect "Section 19: full form flow on local checkout fixture"
SC=$($LOOM session create --profile standard 2>&1 | jq -r .session_id)
nav    "$SC" "$CHECKOUT_URL"        >/dev/null
type_  "$SC" '#name'  'Test User'   >/dev/null
type_  "$SC" '#email' 'test@example.com' >/dev/null
type_  "$SC" '#card'  '4242424242424242' >/dev/null
sel    "$SC" '#country' 'GB'        >/dev/null
click  "$SC" '#book'                >/dev/null
CONF=$(ev "$SC" 'document.getElementById("confirmation").textContent' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
if echo "$CONF" | grep -q "Booked: Test User"; then
  ok "checkout-flow-end-to-end"
else
  fail "checkout-flow-end-to-end" "got '$CONF'"
fi
$LOOM session close "$SC" >/dev/null 2>&1 || true

# -- Section 20: real public site ---------------------------------------
sect "Section 20: real public site (example.com — sanity check)"
SR=$($LOOM session create --profile standard 2>&1 | jq -r .session_id)
nav "$SR" 'https://example.com' >/dev/null
TITLE=$(ev "$SR" 'document.title' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
if [ "$TITLE" = "Example Domain" ]; then
  ok "example-com-loads"
else
  fail "example-com-loads" "got '$TITLE'"
fi
$LOOM session close "$SR" >/dev/null 2>&1 || true
