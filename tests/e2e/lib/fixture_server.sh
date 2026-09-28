# shellcheck shell=bash
# tests/e2e/lib/fixture_server.sh — start the fixtures/ HTTP server and wait
# until it answers. Sourced by run_e2e.sh, run_load.sh and run_mcp.sh.
#
# start_fixture_server <port> <log-file>
#   Serves ./fixtures on 127.0.0.1:<port> (the address every fixture URL
#   names), unbuffered so <log-file> says why a start failed. It is polled, not
#   slept on: a cold macOS runner takes longer than one second to start Python,
#   and a single curl after `sleep 1` failed that CI job on every run.
#   Sets FIXTURE_PID and returns 0 once the server answers; returns 1 when it
#   exits or stays silent for ~30 s.
start_fixture_server() {
  local port="$1" log="$2"
  python3 -u -m http.server "$port" --bind 127.0.0.1 --directory fixtures >"$log" 2>&1 &
  FIXTURE_PID=$!
  for _ in $(seq 1 60); do
    curl -sf --max-time 2 "http://127.0.0.1:${port}/index.html" >/dev/null && return 0
    kill -0 "$FIXTURE_PID" 2>/dev/null || return 1 # the server exited: stop waiting
    sleep 0.5
  done
  return 1
}
