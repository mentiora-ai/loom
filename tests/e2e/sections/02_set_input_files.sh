# shellcheck shell=bash
# tests/e2e/sections/02_set_input_files.sh — Section 11b: web.set_input_files.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

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
