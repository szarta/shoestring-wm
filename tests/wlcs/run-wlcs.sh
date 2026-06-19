#!/usr/bin/env bash
# Run the full WLCS conformance suite against the shoestring-wm plugin and gate
# the result against the tracked xfail skip-list (see check-results.py).
#
# Usage: tests/wlcs/run-wlcs.sh <path-to-libwlcs_shoestring.so>
#
# Each test runs in its OWN `wlcs` process under a timeout. Per-test isolation
# bounds each failure (a hanging test can't take the suite down) — kept even
# though the old multi-second hangs are gone since the eager-flush (task 146)
# and layer frame-callback (task 133) fixes; a full serial run is now ~55s with
# zero timeouts.
#
# Runs SERIALLY by default (WLCS_JOBS=1). Parallelism (the old nproc*4 default)
# made the suite non-deterministic run-to-run: each test spawns its own wlcs
# process + compositor thread, and CPU contention reshuffled timing enough to
# flip ~5-9 timing-sensitive tests pass<->fail between runs (task 155), which a
# CI gate (check-results.py vs the skip-list) reads as phantom regressions.
# Serial trades a faster wall-clock for a stable verdict — the right call for a
# gate. Override with WLCS_JOBS=N to parallelise locally (accepts the variance).
#
# The `wlcs` runner must be on $PATH (or set $WLCS). Tunables:
#   $WLCS_TIMEOUT  per-test timeout seconds (default 20)
#   $WLCS_JOBS     concurrent jobs (default: 1 = serial/deterministic)
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
jobs="${WLCS_JOBS:-1}"
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
mode=$([ "$jobs" -eq 1 ] && echo "serial" || echo "${jobs}-way parallel")
echo "running $(wc -l < "$tests") WLCS tests (timeout ${timeout_s}s, ${mode})…"

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

# The flaky-list is optional; pass it through only when present.
flaky="$here/flaky-list.txt"
if [ -f "$flaky" ]; then
    exec python3 "$here/check-results.py" "$results" "$here/skip-list.txt" "$flaky"
else
    exec python3 "$here/check-results.py" "$results" "$here/skip-list.txt"
fi
