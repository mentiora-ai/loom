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

# shellcheck source=lib/e2e_helpers.sh
source "$HERE/lib/e2e_helpers.sh"
# shellcheck source=lib/fixture_server.sh
source "$HERE/lib/fixture_server.sh"

# -- Fixture server -----------------------------------------------------
sect "Booting fixture HTTP server on :${FIXTURE_PORT}"
trap 'kill ${FIXTURE_PID:-} 2>/dev/null || true' EXIT
if ! start_fixture_server "$FIXTURE_PORT" "$RESULTS/fixture-server.log"; then
  fail "fixture-server-up" "couldn't curl $FIXTURE_URL; server log: $(tail -5 "$RESULTS/fixture-server.log")"
  exit 1
fi
ok "fixture-server-up"

# The sections run in order in this one shell: later ones reuse the session
# Section 1 creates, and `exit 1` in any of them ends the whole run.
source "$HERE/sections/01_core_verbs.sh"
source "$HERE/sections/02_set_input_files.sh"
source "$HERE/sections/03_fill_dates.sh"
source "$HERE/sections/04_guest_locators.sh"
source "$HERE/sections/05_session_lifecycle.sh"
source "$HERE/sections/06_errors_budgets.sh"
source "$HERE/sections/07_parallel_flows.sh"
source "$HERE/sections/08_determinism.sh"

# -- Summary ------------------------------------------------------------
sect "Summary"
TOTAL=$((PASS+FAIL))
printf '  %d / %d passed\n' "$PASS" "$TOTAL"
if [ "$FAIL" -gt 0 ]; then
  printf '\nFailed:\n'
  for f in "${FAILED[@]}"; do printf '  - %s\n' "$f"; done
  exit 1
fi
