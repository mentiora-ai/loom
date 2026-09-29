# shellcheck shell=bash
# tests/e2e/sections/04_guest_locators.sh — Section 11d: the guest-path verbs take the locator grammar.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

# -- Section 11d: guest-path verbs take the locator grammar ---------------------
# web.select, web.hover, web.scroll and web.type mode:"value" resolve their
# target in the page. They used to hand the selector to document.querySelector
# verbatim, so css=/text=/role=/frame= threw js_throw; they now resolve through
# the same grammar as web.click (a plain CSS selector keeps its exact payload).
sect "Section 11d: guest verbs — css=/text=/role=/frame= locators"
LSESSION=$($LOOM session create --profile standard 2>&1 | jq -r .session_id 2>/dev/null)
if [[ "$LSESSION" =~ ^[a-z0-9]{26}$ ]]; then
  nav "$LSESSION" "$LOCATORS_URL" >/dev/null
  lval() { ev "$LSESSION" "$1" | jq -r '.return_value_json // empty' | jq -r 'select(. != null)'; }

  SR=$(sel "$LSESSION" 'role=combobox[name="Size"]' 'l')
  echo "$SR" >"$RESULTS/select-role.json"
  if [ "$(lval 'document.getElementById("size").value')" = "l" ] && ! echo "$SR" | grep -q '"status":"error"'; then
    ok "select-by-role-locator"
  else
    fail "select-by-role-locator" "see $RESULTS/select-role.json"
  fi

  SC=$(sel "$LSESSION" 'css=#size' 'm')
  echo "$SC" >"$RESULTS/select-css.json"
  if [ "$(lval 'document.getElementById("size").value')" = "m" ]; then
    ok "select-by-css-prefixed-locator"
  else
    fail "select-by-css-prefixed-locator" "see $RESULTS/select-css.json"
  fi

  SF=$(sel "$LSESSION" 'frame=#frame >> css=#inner-size' 'l')
  echo "$SF" >"$RESULTS/select-frame.json"
  if [ "$(lval 'document.getElementById("frame").contentDocument.getElementById("inner-size").value')" = "l" ]; then
    ok "select-in-same-origin-frame"
  else
    fail "select-in-same-origin-frame" "see $RESULTS/select-frame.json"
  fi

  VT=$($LOOM action web.type --session "$LSESSION" --selector 'role=textbox[name="Nickname"]' --text 'new' --mode value 2>&1)
  echo "$VT" >"$RESULTS/type-value-role.json"
  if [ "$(lval 'document.getElementById("nick").value')" = "new" ]; then
    ok "type-value-mode-by-role-locator"
  else
    fail "type-value-mode-by-role-locator" "see $RESULTS/type-value-role.json"
  fi

  HV=$(hover "$LSESSION" 'text=Hover me')
  echo "$HV" >"$RESULTS/hover-text.json"
  if [ "$(lval 'window.__hovered')" = "1" ]; then
    ok "hover-by-text-locator"
  else
    fail "hover-by-text-locator" "see $RESULTS/hover-text.json"
  fi

  SCR=$(scroll "$LSESSION" 'css=#feed' 100)
  echo "$SCR" >"$RESULTS/scroll-css.json"
  if [ "$(lval 'document.getElementById("feed").scrollTop')" = "100" ]; then
    ok "scroll-by-css-prefixed-locator"
  else
    fail "scroll-by-css-prefixed-locator" "see $RESULTS/scroll-css.json"
  fi

  # A selector Chromium cannot parse (Playwright's :text(), as a studio agent
  # wrote on hollie staging) is a typed miss, and the session's browser survives.
  # It used to be recorded as a transport failure that shut the browser down, so
  # every later call in the session failed.
  IS=$(click "$LSESSION" "li:has(span:text('Hover me')) button")
  echo "$IS" >"$RESULTS/invalid-selector.json"
  if echo "$IS" | grep -q 'selector_not_found' && [ "$(lval 'document.title')" = "Loom E2E Locators Fixture" ]; then
    ok "invalid-selector-is-a-miss-and-the-session-survives"
  else
    fail "invalid-selector-is-a-miss-and-the-session-survives" "see $RESULTS/invalid-selector.json"
  fi

  $LOOM session close "$LSESSION" >/dev/null 2>&1 || true
else
  fail "locators-session-create" "could not create locators session"
fi
