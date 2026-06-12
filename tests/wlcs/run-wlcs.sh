#!/usr/bin/env bash
# Run the full WLCS conformance suite against the shoestring-wm plugin and gate
# the result against the tracked xfail skip-list (see check-results.py).
#
# Usage: tests/wlcs/run-wlcs.sh <path-to-libwlcs_shoestring.so>
#
# Each test runs in its OWN `wlcs` process under a timeout, in parallel. This is
# deliberate: many tests for features we don't implement block on a WLCS
# internal wait, and running the whole suite in one process serializes those
# multi-second waits into an effective hang. Per-test isolation bounds each
# failure and lets independent tests overlap.
#
# The `wlcs` runner must be on $PATH (or set $WLCS). Tunables:
#   $WLCS_TIMEOUT  per-test timeout seconds (default 20)
#   $WLCS_JOBS     parallel jobs (default: nproc*4 — failing tests mostly sleep)
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
plugin="${1:?usage: run-wlcs.sh <plugin.so>}"

wlcs_bin="${WLCS:-wlcs}"
if ! command -v "$wlcs_bin" >/dev/null 2>&1 && [ ! -x "$wlcs_bin" ]; then
    echo "error: '$wlcs_bin' not found — install WLCS or set \$WLCS" >&2
    exit 2
fi
[ -e "$plugin" ] || { echo "error: plugin '$plugin' does not exist (build it first)" >&2; exit 2; }
plugin="$(cd "$(dirname "$plugin")" && pwd)/$(basename "$plugin")"   # absolutize

timeout_s="${WLCS_TIMEOUT:-20}"
jobs="${WLCS_JOBS:-$(( $(nproc) * 4 ))}"
results="${WLCS_RESULTS:-$here/results.txt}"

# Enumerate every (non-DISABLED) full test name from gtest's listing. A line
# with no leading space is a suite (`Suite.`); indented lines are cases, which
# may carry a trailing `# GetParam()` comment to strip.
tests="$(mktemp)"
"$wlcs_bin" "$plugin" --gtest_list_tests 2>/dev/null | awk '
  /^[^[:space:]]/ { suite=$1; next }
  /^[[:space:]]/  { name=$1; sub(/#.*/,"",name); gsub(/[[:space:]]/,"",name);
                    if (name!="" && name !~ /DISABLED_/) print suite name }
' > "$tests"
echo "running $(wc -l < "$tests") WLCS tests (timeout ${timeout_s}s, ${jobs} parallel)…"

run_one() {
    local name="$1"
    local out rc
    out="$(timeout "$timeout_s" "$WLCS_BIN" "$WLCS_PLUGIN" --gtest_filter="$name" 2>&1)"
    rc=$?
    if [ "$rc" -eq 124 ]; then echo -e "TIMEOUT\t$name"
    elif [ "$rc" -eq 0 ] && grep -q '\[  PASSED  \] 1 test' <<<"$out"; then echo -e "PASS\t$name"
    else echo -e "FAIL\t$name"; fi
}
export -f run_one
export WLCS_BIN="$wlcs_bin" WLCS_PLUGIN="$plugin" timeout_s

xargs -d '\n' -P "$jobs" -I{} bash -c 'run_one "$@"' _ {} < "$tests" > "$results"
rm -f "$tests"

exec python3 "$here/check-results.py" "$results" "$here/skip-list.txt"
