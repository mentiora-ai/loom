#!/usr/bin/env bash
# tests/e2e/run_mcp.sh — drive `loom-mcp serve` over stdio and exercise the
# tool surface end-to-end.
#
# Spawns the MCP server, sends initialize → tools/list → tools/call and
# verifies each promised tool surfaces with a typed result.

set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE="$(cd "$HERE/../.." && pwd)"
cd "$HERE"

MCP="${LOOM_MCP_BIN:-$WORKSPACE/target/release/loom-mcp}"
RESULTS=results
FIXTURE_PORT="${FIXTURE_PORT:-8765}"
FIXTURE_URL="http://127.0.0.1:${FIXTURE_PORT}/index.html"

mkdir -p "$RESULTS"

# Boot the fixture server if not already up
if ! curl -sf "$FIXTURE_URL" >/dev/null; then
  # shellcheck source=lib/fixture_server.sh
  source "$HERE/lib/fixture_server.sh"
  trap 'kill ${FIXTURE_PID:-} 2>/dev/null || true' EXIT
  if ! start_fixture_server "$FIXTURE_PORT" "$RESULTS/mcp-fixture.log"; then
    echo "fixture server did not come up on :$FIXTURE_PORT:" >&2
    tail -5 "$RESULTS/mcp-fixture.log" >&2
    exit 1
  fi
fi

# Build a sequence of NDJSON requests
REQ_FILE="$RESULTS/mcp-req.ndjson"
RES_FILE="$RESULTS/mcp-res.ndjson"
{
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"loom-e2e","version":"0"}}}'
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"loom.web.navigate","arguments":{"url":"'"$FIXTURE_URL"'"}}}'
  echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"loom.web.evaluate","arguments":{"expression":"document.title"}}}'
  echo '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"loom.web.click","arguments":{"selector":"#ok-button"}}}'
  echo '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"loom.web.evaluate","arguments":{"expression":"document.getElementById(\"result\").textContent"}}}'
  echo '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"loom.web.snapshot","arguments":{}}}'
} >"$REQ_FILE"

# Run with a 30s timeout — server reads stdin until EOF then exits.
# `timeout` is GNU coreutils; macOS doesn't ship it. Fall back to
# `gtimeout` (brew coreutils) or run without a timeout (the server
# exits on EOF anyway, so the timeout is purely a stuck-process
# safety net). Branching on the wrapper itself (rather than building
# an array) avoids bash 3.2's `set -u` empty-array unbound-variable
# trap on macOS.
if command -v timeout >/dev/null 2>&1; then
  timeout 30 "$MCP" serve <"$REQ_FILE" >"$RES_FILE" 2>"$RESULTS/mcp-server.log" || true
elif command -v gtimeout >/dev/null 2>&1; then
  gtimeout 30 "$MCP" serve <"$REQ_FILE" >"$RES_FILE" 2>"$RESULTS/mcp-server.log" || true
else
  "$MCP" serve <"$REQ_FILE" >"$RES_FILE" 2>"$RESULTS/mcp-server.log" || true
fi

echo "--- requests sent ---"
nl "$REQ_FILE"
echo
echo "--- responses received ---"
nl "$RES_FILE"
echo
echo "--- server stderr ---"
tail -30 "$RESULTS/mcp-server.log"

# Pass criteria
PASS=0; FAIL=0
declare -a F
ok()   { PASS=$((PASS+1)); printf '  PASS %s\n' "$1"; }
fail() { FAIL=$((FAIL+1)); F+=("$1"); printf '  FAIL %s — %s\n' "$1" "$2"; }

# Expect at least 6 responses (initialize, tools/list, navigate, evaluate, click, evaluate, snapshot ≈ 7; notifications/initialized has no response)
N=$(wc -l <"$RES_FILE" | tr -d ' ')
if [ "$N" -ge 6 ]; then ok "received-N-responses ($N)"; else fail "received-N-responses" "$N"; fi

# tools/list must contain loom.web.* names
TOOLS=$(grep -m1 '"id":2' "$RES_FILE" | jq -r '.result.tools[].name' 2>/dev/null | sort | tr '\n' ' ')
echo "  tools advertised: $TOOLS"
for needed in loom.web.navigate loom.web.evaluate loom.web.click loom.web.type loom.web.wait; do
  if echo "$TOOLS" | grep -q "$needed"; then ok "tools-list-has $needed"; else fail "tools-list-has" "$needed missing"; fi
done

# tools/call navigate must have content with action_hash
NAV=$(grep -m1 '"id":3' "$RES_FILE")
if echo "$NAV" | grep -qE '"isError":(false)|action_hash'; then
  ok "navigate-tool-returns-non-error"
else
  fail "navigate-tool-returns-non-error" "$NAV"
fi

# evaluate document.title (id 4) should give "Loom E2E Fixture" somewhere
EV=$(grep -m1 '"id":4' "$RES_FILE")
if echo "$EV" | grep -q "Loom E2E Fixture"; then
  ok "evaluate-tool-returns-title"
else
  fail "evaluate-tool-returns-title" "$EV"
fi

# click → evaluate result should give "clicked"
EV2=$(grep -m1 '"id":6' "$RES_FILE")
# return_value_json is JSON-string-encoded, so the wire shows "clicked" with escaped quotes
if echo "$EV2" | grep -qE 'return_value_json.{0,30}clicked'; then
  ok "click-fired-via-mcp"
else
  fail "click-fired-via-mcp" "$EV2"
fi

echo
TOTAL=$((PASS+FAIL))
echo "MCP: $PASS / $TOTAL passed"
if [ "$FAIL" -gt 0 ]; then
  for x in "${F[@]}"; do echo "  - $x"; done
  exit 1
fi
