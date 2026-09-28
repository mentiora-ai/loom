# shellcheck shell=bash
# tests/e2e/sections/07_errors_budgets.sh — Sections 16–17: typed errors and budget enforcement.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

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
