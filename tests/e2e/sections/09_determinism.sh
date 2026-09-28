# shellcheck shell=bash
# tests/e2e/sections/09_determinism.sh — Section 21: cross-run determinism.
# Sourced by run_e2e.sh, in order: uses its config, the helpers in
# lib/e2e_helpers.sh and the state earlier sections set.

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
