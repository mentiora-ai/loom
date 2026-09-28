#!/usr/bin/env bash
# tests/e2e/run_e2e.sh — comprehensive real-world test of the loom runtime.
#
# Tests every README-promised feature end-to-end: navigate, click, type,
# wait, evaluate, screenshot, snapshot, replay, validate, parallel sessions,
# typed errors, budgets, time-travel inspect.
#
# Usage:  bash tests/e2e/run_e2e.sh
# Requires the daemon to be running (`loom serve`) and `jq` on PATH.

set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$(cd "$HERE/../.." && pwd)"
cd "$HERE"

LOOM="${LOOM_BIN:-$WORKSPACE/target/release/loom}"
RESULTS=results
FIXTURE_PORT="${FIXTURE_PORT:-8765}"
FIXTURE_URL="http://127.0.0.1:${FIXTURE_PORT}/index.html"
CHECKOUT_URL="http://127.0.0.1:${FIXTURE_PORT}/checkout.html"
UPLOAD_URL="http://127.0.0.1:${FIXTURE_PORT}/upload.html"
DATES_URL="http://127.0.0.1:${FIXTURE_PORT}/dates.html"
LOCATORS_URL="http://127.0.0.1:${FIXTURE_PORT}/locators.html"
# Absolute fixtures dir — the daemon under test MUST be started with
# LOOM_UPLOAD_ROOT set to this path for the web.set_input_files happy-path
# to pass (fail-closed otherwise). The harness asserts that contract.
# Use $HERE (resolved above) — the script has already `cd "$HERE"`, so a
# $0-relative recompute would double-nest.
FIXTURES_DIR="$HERE/fixtures"
UPLOAD_FILE="$FIXTURES_DIR/sample-upload.txt"

mkdir -p "$RESULTS"
PASS=0; FAIL=0
declare -a FAILED

ok()   { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { FAIL=$((FAIL+1)); FAILED+=("$1"); printf '  \033[31mFAIL\033[0m %s\n' "$1"; [ -n "${2:-}" ] && printf '       %s\n' "$2"; }
sect() { printf '\n\033[36m== %s ==\033[0m\n' "$1"; }

# Run an action through loom, suppressing the noisy unmatched-prefix
# stderr line from the daemon when the script's quoting is right.
nav()    { $LOOM action web.navigate   --session "$1" --url "$2" 2>&1; }
ev()     { $LOOM action web.evaluate   --session "$1" --expression "$2" 2>&1; }
type_()  { $LOOM action web.type       --session "$1" --selector "$2" --text "$3" 2>&1; }
click()  { $LOOM action web.click      --session "$1" --selector "$2" 2>&1; }
sel()    { $LOOM action web.select     --session "$1" --selector "$2" --value "$3" 2>&1; }
wait_()  { $LOOM action web.wait       --session "$1" --selector "$2" --timeout_ms "${3:-3000}" 2>&1; }
hover()  { $LOOM action web.hover      --session "$1" --selector "$2" 2>&1; }
scroll() { $LOOM action web.scroll     --session "$1" --selector "$2" --delta_y "${3:-100}" 2>&1; }
shot()   { $LOOM action web.screenshot --session "$1" 2>&1; }
snap()   { $LOOM action web.snapshot   --session "$1" 2>&1; }
upload() { $LOOM action web.set_input_files --session "$1" --selector "$2" --paths "$3" 2>&1; }

# -- Fixture server -----------------------------------------------------
sect "Booting fixture HTTP server on :${FIXTURE_PORT}"
python3 -m http.server "$FIXTURE_PORT" --directory fixtures >"$RESULTS/fixture-server.log" 2>&1 &
FIXTURE_PID=$!
trap 'kill $FIXTURE_PID 2>/dev/null || true' EXIT
sleep 1
if ! curl -sf "$FIXTURE_URL" >/dev/null; then
  fail "fixture-server-up" "couldn't curl $FIXTURE_URL"
  exit 1
fi
ok "fixture-server-up"

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

# -- Section 11b: web.set_input_files -----------------------------------
# Requires the daemon under test to be started with
# LOOM_UPLOAD_ROOT="$FIXTURES_DIR" (fail-closed otherwise). The happy path
# uploads a fixture file into a real <input type=file> and reads back the
# FileList via web.evaluate; the negative cases assert typed errors.
sect "Section 11b: web.set_input_files"
UPSESSION=$($LOOM session create --profile standard 2>&1 | jq -r .session_id 2>/dev/null)
if [[ "$UPSESSION" =~ ^[a-z0-9]{26}$ ]]; then
  nav "$UPSESSION" "$UPLOAD_URL" >/dev/null
  # Happy path: upload the fixture, then read input.files via web.evaluate.
  UP=$(upload "$UPSESSION" '#upload' "[\"$UPLOAD_FILE\"]")
  echo "$UP" >"$RESULTS/upload.json"
  LEN=$(ev "$UPSESSION" 'document.querySelector("#upload").files.length' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
  NAME=$(ev "$UPSESSION" 'document.querySelector("#upload").files[0] && document.querySelector("#upload").files[0].name' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
  if [ "$LEN" = "1" ] && echo "$NAME" | grep -q 'sample-upload.txt'; then
    ok "set-input-files-filelist-reflects-upload"
  else
    fail "set-input-files-filelist-reflects-upload" "len=$LEN name=$NAME (is the daemon started with LOOM_UPLOAD_ROOT=$FIXTURES_DIR? see $RESULTS/upload.json)"
  fi

  # Locator-grammar happy path: a `css=`-prefixed selector (the documented form
  # web.click/web.type accept) must resolve + attach exactly like a bare one.
  # Regression guard: set_input_files used to pass the selector RAW to
  # DOM.querySelector, so `css=#upload` never resolved and surface_trapped even
  # for a valid file under LOOM_UPLOAD_ROOT.
  CSSUP=$(upload "$UPSESSION" 'css=#upload' "[\"$UPLOAD_FILE\"]")
  echo "$CSSUP" >"$RESULTS/upload-css.json"
  CSSLEN=$(ev "$UPSESSION" 'document.querySelector("#upload").files.length' | jq -r '.return_value_json // empty' | jq -r 'select(. != null)')
  if [ "$CSSLEN" = "1" ] && ! echo "$CSSUP" | grep -qiE 'surface_trap|action dispatch failed'; then
    ok "set-input-files-css-locator-grammar-attaches"
  else
    fail "set-input-files-css-locator-grammar-attaches" "len=$CSSLEN (see $RESULTS/upload-css.json) — css= selector must resolve like web.click/web.type"
  fi

  # Negative: a path outside the allow-list root → typed security error.
  BLK=$(upload "$UPSESSION" '#upload' '["/etc/passwd"]')
  echo "$BLK" >"$RESULTS/upload-blocked.json"
  if echo "$BLK" | grep -qiE 'upload_path_blocked|upload_root_not_configured'; then
    ok "set-input-files-path-outside-root-blocked"
  else
    fail "set-input-files-path-outside-root-blocked" "see $RESULTS/upload-blocked.json"
  fi

  # Selector miss → typed selector_not_found.
  MISS=$(upload "$UPSESSION" '#no-such-input-zzz' "[\"$UPLOAD_FILE\"]")
  if echo "$MISS" | grep -qiE 'selector_not_found|selector-not-found'; then
    ok "set-input-files-selector-miss-typed-error"
  else
    fail "set-input-files-selector-miss-typed-error" "$MISS"
  fi

  # Wrong element type (text input, not a file input) → not_a_file_input.
  WRONG=$(upload "$UPSESSION" '#text-field' "[\"$UPLOAD_FILE\"]")
  if echo "$WRONG" | grep -qiE 'not_a_file_input|not-a-file-input'; then
    ok "set-input-files-wrong-element-typed-error"
  else
    fail "set-input-files-wrong-element-typed-error" "$WRONG"
  fi

  $LOOM session close "$UPSESSION" >/dev/null 2>&1 || true
else
  fail "set-input-files-session-create" "could not create upload session"
fi

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

  $LOOM session close "$LSESSION" >/dev/null 2>&1 || true
else
  fail "locators-session-create" "could not create locators session"
fi

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

# -- Section 16: typed errors -------------------------------------------
sect "Section 16: typed errors"
S2=$($LOOM session create --profile standard 2>&1 | jq -r .session_id)

DNS_ERR=$(nav "$S2" 'http://this-host-does-not-exist-loom-test.invalid/')
echo "$DNS_ERR" >"$RESULTS/dns-error.json"
if echo "$DNS_ERR" | grep -qiE 'dns_failure|dns-failure|ERR_NAME_NOT_RESOLVED'; then
  ok "dns-failure-typed-error"
else
  fail "dns-failure-typed-error" "see $RESULTS/dns-error.json"
fi

HTTP_ERR=$(nav "$S2" "${FIXTURE_URL%/index.html}/no-such-path-404")
echo "$HTTP_ERR" >"$RESULTS/http-error.json"
if echo "$HTTP_ERR" | grep -qiE 'http_status|http-status|"status_code":404|404'; then
  ok "http-status-typed-error"
else
  fail "http-status-typed-error" "see $RESULTS/http-error.json"
fi

WPF=$(wait_ "$S2" '#nonexistent-selector-zzz' 500)
echo "$WPF" >"$RESULTS/wait-error.json"
if echo "$WPF" | grep -qiE 'wait_predicate_false|wait-predicate-false'; then
  ok "wait-predicate-false-typed-error"
else
  fail "wait-predicate-false-typed-error" "see $RESULTS/wait-error.json"
fi

URL_BLK=$(nav "$S2" 'javascript:alert(1)')
if echo "$URL_BLK" | grep -qiE 'url_blocked|url-blocked|scheme'; then
  ok "url-blocked-typed-error"
else
  fail "url-blocked-typed-error" "$URL_BLK"
fi

$LOOM session close "$S2" >/dev/null 2>&1 || true

# -- Section 17: budget enforcement -------------------------------------
sect "Section 17: budgets"
S3=$($LOOM session create --profile standard --budget wall_clock=2s 2>&1 | jq -r .session_id 2>/dev/null)
if [[ "$S3" =~ ^[a-z0-9]{26}$ ]]; then
  ok "session-with-wallclock-budget-creates"
  nav "$S3" "$FIXTURE_URL" >/dev/null
  sleep 3
  AFTER=$(ev "$S3" '1+1')
  echo "$AFTER" >"$RESULTS/budget-after.json"
  if echo "$AFTER" | grep -qiE 'budget_exceeded|budget-exceeded|budget|exceeded|expired'; then
    ok "budget-exceeded-typed-error"
  else
    fail "budget-exceeded-typed-error" "see $RESULTS/budget-after.json"
  fi
  $LOOM session close "$S3" >/dev/null 2>&1 || true
else
  fail "session-with-wallclock-budget-creates" "could not create"
fi

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

# -- Section 21: cross-run determinism ----------------------------------
# Two INDEPENDENT fresh recordings of the same actions, with the same
# --seed AND --clock-anchor, must diff field_diffs=0 (incl. dom_snapshot_hash).
# This is the headline cross-run claim (Section 15 proves self-replay; this
# proves two-fresh-runs). The fixture's setTimeout(...,200) reveal exercises
# the virtual-time-budget settle; Date.now()/Math.random() exercise anchor+seed.
sect "Section 21: cross-run determinism (--seed + --clock-anchor → field_diffs=0)"
ANCHOR=1700000000000
xrun() {  # $1 = label → echoes the session id after recording the fixed action sequence
  local sid
  sid=$($LOOM session create --profile standard --seed 42 --clock-anchor "$ANCHOR" 2>&1 | jq -r .session_id 2>/dev/null)
  nav "$sid" "$FIXTURE_URL" >/dev/null
  ev "$sid" 'JSON.stringify(window.__loomFixture)' >/dev/null
  $LOOM session close "$sid" >/dev/null 2>&1 || true
  echo "$sid"
}
S_A=$(xrun A)
S_B=$(xrun B)
if [[ "$S_A" =~ ^[a-z0-9]{26}$ ]] && [[ "$S_B" =~ ^[a-z0-9]{26}$ ]]; then
  ok "cross-run-sessions-created ($S_A, $S_B)"
  XDIFF=$($LOOM session diff "$S_A" "$S_B" 2>&1)
  echo "$XDIFF" >"$RESULTS/xrun-diff.json"
  XFD=$(echo "$XDIFF" | jq -r '.field_diffs | length' 2>/dev/null)
  if [ "$XFD" = "0" ]; then
    ok "cross-run-field-diffs-zero (two fresh --seed+--clock-anchor runs are identical)"
  else
    fail "cross-run-field-diffs-zero" "field_diffs=$XFD — see $RESULTS/xrun-diff.json"
  fi
  # Negative control: a fresh session WITHOUT --clock-anchor must diverge from
  # S_A (proves the anchor is load-bearing, not a no-op).
  S_C=$($LOOM session create --profile standard --seed 42 2>&1 | jq -r .session_id 2>/dev/null)
  nav "$S_C" "$FIXTURE_URL" >/dev/null
  ev "$S_C" 'JSON.stringify(window.__loomFixture)' >/dev/null
  $LOOM session close "$S_C" >/dev/null 2>&1 || true
  CDIFF=$($LOOM session diff "$S_A" "$S_C" 2>&1)
  CFD=$(echo "$CDIFF" | jq -r '.field_diffs | length' 2>/dev/null)
  if [ "${CFD:-0}" -gt 0 ] 2>/dev/null; then
    ok "no-anchor-control-diverges (field_diffs=$CFD > 0)"
  else
    fail "no-anchor-control-diverges" "expected field_diffs>0 without --clock-anchor, got '$CFD'"
  fi
else
  fail "cross-run-sessions-created" "S_A='$S_A' S_B='$S_B'"
fi

# -- Summary ------------------------------------------------------------
sect "Summary"
TOTAL=$((PASS+FAIL))
printf '  %d / %d passed\n' "$PASS" "$TOTAL"
if [ "$FAIL" -gt 0 ]; then
  printf '\nFailed:\n'
  for f in "${FAILED[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
