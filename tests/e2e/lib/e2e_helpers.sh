# shellcheck shell=bash
# tests/e2e/lib/e2e_helpers.sh — pass/fail bookkeeping and one-line verb
# wrappers for run_e2e.sh and its sections. Sourced, never run.

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
