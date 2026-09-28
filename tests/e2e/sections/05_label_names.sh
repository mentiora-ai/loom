# shellcheck shell=bash
# tests/e2e/sections/05_label_names.sh — Section 11e: role= names a control by the <label> wrapping it.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

# -- Section 11e: role= names from a wrapping <label> ---------------------------
# A control nested in its <label> (no for=, no id) takes the label's own text as
# its accessible name, as in Playwright and the W3C AccName rules. The resolver
# used to read only <label for=…> and placeholders, so role=textbox[name="Email
# address"] could not find hollie's Team field, and a wrapped <select> was named
# after its options. Checked on both resolver paths: web.type fill (host) and
# web.select (guest).
sect "Section 11e: role= names from a wrapping <label>"
WSESSION=$($LOOM session create --profile standard 2>&1 | jq -r .session_id 2>/dev/null)
if [[ "$WSESSION" =~ ^[a-z0-9]{26}$ ]]; then
  nav "$WSESSION" "$LABELS_URL" >/dev/null
  wval() { ev "$WSESSION" "$1" | jq -r '.return_value_json // empty' | jq -r 'select(. != null)'; }

  WE=$(type_ "$WSESSION" 'role=textbox[name="Work email"]' 'e2e@example.com')
  echo "$WE" >"$RESULTS/label-wrapped-email.json"
  if [ "$(wval 'document.querySelector("#wrapped-email input").value')" = "e2e@example.com" ]; then
    ok "type-into-textbox-named-by-wrapping-label"
  else
    fail "type-into-textbox-named-by-wrapping-label" "see $RESULTS/label-wrapped-email.json"
  fi

  WP=$(sel "$WSESSION" 'role=combobox[name="Plan"]' 'b')
  echo "$WP" >"$RESULTS/label-wrapped-plan.json"
  if [ "$(wval 'document.querySelector("#wrapped-plan select").value')" = "b" ]; then
    ok "select-combobox-named-by-wrapping-label"
  else
    fail "select-combobox-named-by-wrapping-label" "see $RESULTS/label-wrapped-plan.json"
  fi

  # The wrapped <select>'s options are not its name: "Alpha" must not resolve it.
  WO=$(sel "$WSESSION" 'role=combobox[name="Alpha"]' 'a')
  echo "$WO" >"$RESULTS/label-option-text.json"
  if echo "$WO" | grep -q '"status":"error"' && [ "$(wval 'document.querySelector("#wrapped-plan select").value')" = "b" ]; then
    ok "wrapped-select-not-named-by-its-options"
  else
    fail "wrapped-select-not-named-by-its-options" "see $RESULTS/label-option-text.json"
  fi

  # <label for=…> still names its control.
  WX=$(type_ "$WSESSION" 'role=textbox[name="Explicit field"]' 'still here')
  echo "$WX" >"$RESULTS/label-explicit.json"
  if [ "$(wval 'document.getElementById("explicit").value')" = "still here" ]; then
    ok "label-for-still-names-its-control"
  else
    fail "label-for-still-names-its-control" "see $RESULTS/label-explicit.json"
  fi

  $LOOM session close "$WSESSION" >/dev/null 2>&1 || true
else
  fail "labels-session-create" "could not create labels session"
fi
