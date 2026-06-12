#!/usr/bin/env python3
"""Apply xfail skip-list semantics to a WLCS per-test results file.

run-wlcs.sh executes each WLCS test in its own process (so one hang can't
freeze the suite) and writes a results file: one `STATUS<TAB>name` line per
test, STATUS in {PASS, FAIL, TIMEOUT}. TIMEOUT counts as a failure. This script
gates the run against the tracked skip-list so CI stays meaningful as coverage
grows:

  * failed (FAIL/TIMEOUT) and NOT on the skip-list -> unexpected failure
    (regression, or a newly-run test we don't pass). Exit non-zero.
  * PASSED but IS on the skip-list -> unexpected pass (we fixed it; remove the
    entry so it can't silently regress). Exit non-zero.
  * failed and on the skip-list -> expected (xfail). Reported, not fatal.

Skip-list format (tests/wlcs/skip-list.txt): one gtest full test name per line
(`Suite.Case`, or parameterized `Suite/Case.Param/0`). Blank lines and lines
starting with `#` are ignored; an inline `# reason` after a name is stripped.

Usage: check-results.py <results.txt> <skip-list.txt>
"""
import sys


def load_skiplist(path):
    skip = {}
    with open(path, encoding="utf-8") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            name = line.split("#", 1)[0].strip()
            if name:
                skip[name] = (line.split("#", 1)[1].strip() if "#" in line else "")
    return skip


def load_results(path):
    """Return {name: passed_bool} from a `STATUS<TAB>name` results file."""
    results = {}
    with open(path, encoding="utf-8") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            if not line.strip():
                continue
            status, _, name = line.partition("\t")
            if not name:
                continue
            results[name] = status.strip() == "PASS"
    return results


def main(argv):
    if len(argv) != 3:
        print(__doc__)
        return 2
    results = load_results(argv[1])
    skip = load_skiplist(argv[2])

    failed = {name for name, ok in results.items() if not ok}
    passed = {name for name, ok in results.items() if ok}

    unexpected_failures = sorted(failed - set(skip))
    unexpected_passes = sorted(name for name in skip if name in passed)
    expected_failures = sorted(failed & set(skip))
    stale_skips = sorted(name for name in skip if name not in results)

    total = len(results)
    print(f"WLCS: {len(passed)}/{total} passed, {len(failed)} failed "
          f"({len(expected_failures)} expected via skip-list)")

    if stale_skips:
        print(f"\nNote: {len(stale_skips)} skip-list entries match no test this "
              f"run (renamed/removed upstream?):")
        for name in stale_skips:
            print(f"  ? {name}")

    if unexpected_passes:
        print(f"\nUNEXPECTED PASSES ({len(unexpected_passes)}) — remove from the skip-list:")
        for name in unexpected_passes:
            print(f"  + {name}")

    if unexpected_failures:
        print(f"\nUNEXPECTED FAILURES ({len(unexpected_failures)}) — "
              f"regressions, or new tests to triage onto the skip-list:")
        for name in unexpected_failures:
            print(f"  ! {name}")

    if unexpected_failures or unexpected_passes:
        print("\nFAIL")
        return 1
    print("\nOK")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
