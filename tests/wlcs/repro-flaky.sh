#!/usr/bin/env bash
# Reproduce the WLCS load-dependent flaky band (task 161) on a dedicated node.
#
# The flakiness is NOT raw CPU starvation of one test (busy-loops + a serial
# single test passes 20/20). It is *multi-process contention*: the gate runs
# each test in its own `wlcs` process, and when many such processes run at once
# every (compositor-thread, test-thread) pair competes for cores. A compositor
# thread descheduled mid configure->ack->resize round-trip leaves the WM arrange
# unsettled when the test's pointer/position check fires -> a real assertion
# fail. So we reproduce by running the flaky subset CONCURRENTLY, many rounds,
# and tally each test's pass rate. A deterministic harness should make every
# test 100% regardless of concurrency.
#
#   ROUNDS=<n>  JOBS=<n>  repro-flaky.sh [optional-test-list-file]
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
PLUGIN="$repo/crates/wlcs-shoestring/target/release/libwlcs_shoestring.so"
WLCS="${WLCS:-$HOME/.local/bin/wlcs}"
ROUNDS="${ROUNDS:-20}"
JOBS="${JOBS:-$(nproc)}"
list="${1:-$here/flaky-list.txt}"

mapfile -t TESTS < <(grep -vE '^[[:space:]]*#' "$list" \
  | grep -vE '^[[:space:]]*$' | sed 's/[[:space:]]*#.*//')

# Job list = every flaky test repeated ROUNDS times, shuffled so a given test's
# reps land in different concurrency neighborhoods.
jobf="$(mktemp)"; results="$(mktemp)"
for r in $(seq 1 "$ROUNDS"); do printf '%s\n' "${TESTS[@]}"; done | shuf > "$jobf"
echo "JOBS=$JOBS concurrent, ROUNDS=$ROUNDS reps x ${#TESTS[@]} tests = $(wc -l < "$jobf") runs"

run_one() {
  local name="$1" out
  out="$(timeout 20 "$WLCS" "$PLUGIN" --gtest_filter="$name" 2>&1)"
  if grep -q '\[  PASSED  \] 1 test' <<<"$out"; then echo -e "PASS\t$name"
  else echo -e "FAIL\t$name"; fi
}
export -f run_one; export WLCS PLUGIN
xargs -d '\n' -P "$JOBS" -I{} bash -c 'run_one "$@"' _ {} < "$jobf" > "$results"

echo
flaky=0
for t in "${TESTS[@]}"; do
  pass=$(grep -cP "^PASS\t\Q$t\E$" "$results")
  tot=$(grep -cP "\t\Q$t\E$" "$results")
  flag=""
  if [ "$pass" -ne 0 ] && [ "$pass" -ne "$tot" ]; then flag=" <-- FLIPS"; flaky=$((flaky+1)); fi
  printf "%3d/%-3d  %s%s\n" "$pass" "$tot" "$t" "$flag"
done
echo
echo "tests that flipped under concurrency: $flaky/${#TESTS[@]}"
rm -f "$jobf" "$results"
