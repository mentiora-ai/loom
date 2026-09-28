# shellcheck shell=bash
# tests/e2e/sections/03_fill_dates.sh — Section 11c: web.type fill on date/time inputs, typed refusals, replace-not-prepend.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

# -- Section 11c: web.type fill on date/time inputs + replace-not-prepend ----
# Regression guards for two fill defects. (1) Chromium ignores Input.insertText
# on <input type=date|time|…>, so fill reported success and left the field
# empty; and role= gave those inputs no role at all. Fill now sets them by
# value (native setter + input/change) and refuses — with a typed error — a
# value the input does not accept or a disabled/readonly target. (2) fill's
# select-all step re-queried the RAW selector with document.querySelector,
# which throws on role=/css=/text=/frame=, so a locator-grammar fill prepended
# to a pre-filled field ("newold").
sect "Section 11c: web.type fill — date/time inputs, typed refusals, replace-not-prepend"
dval() { ev "$1" "$2" | jq -r '.return_value_json // empty' | jq -r 'select(. != null)'; }
idval() { dval "$1" "document.getElementById(\"$2\").value"; }
DSESSION=$($LOOM session create --profile standard 2>&1 | jq -r .session_id 2>/dev/null)
if [[ "$DSESSION" =~ ^[a-z0-9]{26}$ ]]; then
  nav "$DSESSION" "$DATES_URL" >/dev/null

  # A labelled native date input is a textbox to role=, and fill sets it,
  # firing exactly input then change (what a native picker does).
  DT=$(type_ "$DSESSION" 'role=textbox[name="First day"]' '2036-12-31')
  echo "$DT" >"$RESULTS/type-date.json"
  DV=$(idval "$DSESSION" first-day)
  DE=$(dval "$DSESSION" 'window.__events.filter(e => e.startsWith("first-day:")).join(",")')
  if echo "$DT" | jq -e '.action_hash' >/dev/null 2>&1 && [ "$DV" = "2036-12-31" ] && [ "$DE" = "first-day:input,first-day:change" ]; then
    ok "type-date-input-by-role-sets-value"
  else
    fail "type-date-input-by-role-sets-value" "value='$DV' events='$DE' (see $RESULTS/type-date.json)"
  fi

  # Every other set-value input type, by plain CSS, with a value it accepts.
  for pair in 'opens|18:00' 'starts|2036-12-31T18:00' 'billing|2036-12' 'sprint|2036-W52' 'accent|#ff8800' 'volume|40'; do
    id=${pair%%|*}; want=${pair#*|}
    type_ "$DSESSION" "#$id" "$want" >"$RESULTS/type-$id.json"
    got=$(idval "$DSESSION" "$id")
    if [ "$got" = "$want" ]; then
      ok "type-$id-input-sets-value"
    else
      fail "type-$id-input-sets-value" "want '$want' got '$got' (see $RESULTS/type-$id.json)"
    fi
  done

  # A value the input does not accept is a typed error, never a silent success:
  # a reject for EVERY set-value type — a wrong format, or a value the browser
  # normalises or sanitises so the read-back differs (Playwright parity).
  # A refusal changes nothing: the field keeps what it held before.
  for pair in 'role=textbox[name="Last day"]|12/31/2036|last-day|last-day' '#opens|6pm|time|opens' \
              '#starts|2036-12-31 18:00|datetime-local|starts' '#billing|Dec 2036|month|billing' \
              '#sprint|2036-52|week|sprint' '#accent|#FF8800|color-upper|accent' '#volume|abc|range|volume'; do
    selr=${pair%%|*}; rest=${pair#*|}; txt=${rest%%|*}; rest=${rest#*|}; tag=${rest%%|*}; fid=${rest#*|}
    HELD=$(idval "$DSESSION" "$fid")
    BAD=$(type_ "$DSESSION" "$selr" "$txt")
    echo "$BAD" >"$RESULTS/type-malformed-$tag.json"
    AFTER=$(idval "$DSESSION" "$fid")
    if echo "$BAD" | grep -q 'malformed_value' && [ "$AFTER" = "$HELD" ]; then
      ok "type-$tag-malformed-value-typed-error"
    else
      fail "type-$tag-malformed-value-typed-error" "held='$HELD' after='$AFTER' (see $RESULTS/type-malformed-$tag.json)"
    fi
  done

  # An empty text clears a set-value input (one that held a value a moment ago).
  PV=$(idval "$DSESSION" first-day)
  type_ "$DSESSION" 'role=textbox[name="First day"]' '' >"$RESULTS/type-date-clear.json"
  CV=$(idval "$DSESSION" first-day)
  if [ "$PV" = "2036-12-31" ] && [ -z "$CV" ] && ! grep -q '"status":"error"' "$RESULTS/type-date-clear.json"; then
    ok "type-date-input-empty-text-clears"
  else
    fail "type-date-input-empty-text-clears" "before='$PV' after='$CV' (see $RESULTS/type-date-clear.json)"
  fi

  # React-style controlled input: the framework must SEE the change (native
  # prototype setter + input event), not just the DOM value. The control proves
  # the rig can tell: a plain `el.value =` changes the DOM but leaves the
  # tracker blind — the failure a non-native setter would have.
  CTL=$(dval "$DSESSION" 'var c=document.getElementById("tracked-ctl"); c.value="2036-12-31"; c.dispatchEvent(new Event("input",{bubbles:true})); c.value+"|"+window.__trackerChanges["tracked-ctl"].join(",")')
  if [ "$CTL" = "2036-12-31|" ]; then
    ok "value-tracker-control-blind-to-plain-assignment"
  else
    fail "value-tracker-control-blind-to-plain-assignment" "control read '$CTL' — the tracker rig no longer tells the setters apart"
  fi
  type_ "$DSESSION" '#tracked' '2036-12-31' >"$RESULTS/type-date-tracked.json"
  TC=$(dval "$DSESSION" 'window.__trackerChanges.tracked.join(",")')
  if [ "$TC" = "2036-12-31" ]; then
    ok "type-date-input-visible-to-value-tracker"
  else
    fail "type-date-input-visible-to-value-tracker" "tracker saw '$TC' (see $RESULTS/type-date-tracked.json)"
  fi

  # A disabled or readonly target is a typed refusal on BOTH fill paths, and
  # its value is left alone.
  for pair in 'locked|' 'reference|R-1'; do
    id=${pair%%|*}; keep=${pair#*|}
    NE=$(type_ "$DSESSION" "#$id" '2036-01-01')
    echo "$NE" >"$RESULTS/type-not-editable-$id.json"
    got=$(idval "$DSESSION" "$id")
    if echo "$NE" | grep -q 'not_editable' && [ "$got" = "$keep" ]; then
      ok "type-$id-not-editable-typed-error"
    else
      fail "type-$id-not-editable-typed-error" "value='$got' (see $RESULTS/type-not-editable-$id.json)"
    fi
  done

  # Fill REPLACES a pre-filled field through role=, css=, text= and frame= (was "newold").
  type_ "$DSESSION" 'role=textbox[name="Name"]' 'new' >"$RESULTS/type-replace-role.json"
  NV=$(idval "$DSESSION" name)
  if [ "$NV" = "new" ]; then
    ok "type-role-locator-replaces-prefilled-value"
  else
    fail "type-role-locator-replaces-prefilled-value" "value='$NV' (see $RESULTS/type-replace-role.json)"
  fi
  type_ "$DSESSION" 'css=#note' 'x' >"$RESULTS/type-replace-css.json"
  XV=$(idval "$DSESSION" note)
  if [ "$XV" = "x" ]; then
    ok "type-css-locator-replaces-prefilled-value"
  else
    fail "type-css-locator-replaces-prefilled-value" "value='$XV' (see $RESULTS/type-replace-css.json)"
  fi
  type_ "$DSESSION" 'text=hello' 'bye' >"$RESULTS/type-replace-text.json"
  MV=$(idval "$DSESSION" memo)
  if [ "$MV" = "bye" ]; then
    ok "type-text-locator-replaces-prefilled-value"
  else
    fail "type-text-locator-replaces-prefilled-value" "value='$MV' (see $RESULTS/type-replace-text.json)"
  fi
  type_ "$DSESSION" 'frame=#inner-frame >> css=#fname' 'new' >"$RESULTS/type-replace-frame.json"
  FV=$(dval "$DSESSION" 'document.getElementById("inner-frame").contentDocument.getElementById("fname").value')
  if [ "$FV" = "new" ]; then
    ok "type-frame-locator-replaces-prefilled-value"
  else
    fail "type-frame-locator-replaces-prefilled-value" "value='$FV' (see $RESULTS/type-replace-frame.json)"
  fi
  # An empty text clears a text field too (select, then insert "").
  type_ "$DSESSION" 'role=textbox[name="Name"]' '' >"$RESULTS/type-clear-role.json"
  EV=$(idval "$DSESSION" name)
  if [ "$NV" = "new" ] && [ -z "$EV" ]; then
    ok "type-role-locator-empty-text-clears"
  else
    fail "type-role-locator-empty-text-clears" "before='$NV' after='$EV' (see $RESULTS/type-clear-role.json)"
  fi

  # A field the page replaces when it gains focus: an error, never a success
  # typed into a node that has left the document.
  SW=$(type_ "$DSESSION" 'role=textbox[name="Swap me"]' 'lost')
  echo "$SW" >"$RESULTS/type-replaced-node.json"
  if echo "$SW" | grep -q '"status":"error"'; then
    ok "type-replaced-node-is-an-error"
  else
    fail "type-replaced-node-is-an-error" "see $RESULTS/type-replaced-node.json"
  fi

  # The new role reaches the OTHER verbs that share the locator resolver.
  WR=$(wait_ "$DSESSION" 'role=textbox[name="Opens at"]' 3000)
  echo "$WR" >"$RESULTS/wait-role-time.json"
  if echo "$WR" | jq -e '.action_hash' >/dev/null 2>&1 && ! echo "$WR" | grep -q '"status":"error"'; then
    ok "wait-role-textbox-resolves-time-input"
  else
    fail "wait-role-textbox-resolves-time-input" "see $RESULTS/wait-role-time.json"
  fi
  CR=$(click "$DSESSION" 'role=textbox[name="Billing month"]')
  echo "$CR" >"$RESULTS/click-role-month.json"
  if echo "$CR" | jq -e '.action_hash' >/dev/null 2>&1 && ! echo "$CR" | grep -q '"status":"error"'; then
    ok "click-role-textbox-resolves-month-input"
  else
    fail "click-role-textbox-resolves-month-input" "see $RESULTS/click-role-month.json"
  fi

  # No page function reached the typed text through the caller chain.
  LK=$(dval "$DSESSION" 'window.__leaked.join("|")')
  if [ -z "$LK" ]; then
    ok "type-text-not-reachable-via-caller-chain"
  else
    fail "type-text-not-reachable-via-caller-chain" "a page function read: '$LK'"
  fi

  # Shortest accessible name wins: "Date" (a date input, now a textbox) beats
  # "Date of birth" (text) for role=textbox[name="Date"].
  type_ "$DSESSION" 'role=textbox[name="Date"]' '2036-01-02' >"$RESULTS/type-role-shortest.json"
  JD=$(idval "$DSESSION" just-date); DOB=$(idval "$DSESSION" dob)
  if [ "$JD" = "2036-01-02" ] && [ -z "$DOB" ]; then
    ok "type-role-shortest-name-picks-date-input"
  else
    fail "type-role-shortest-name-picks-date-input" "date='$JD' dob='$DOB' (see $RESULTS/type-role-shortest.json)"
  fi

  $LOOM session close "$DSESSION" >/dev/null 2>&1 || true
else
  fail "type-dates-session-create" "could not create dates session"
fi
