#!/usr/bin/env bash
# Measure the headless backend's resource use under fixed workloads
# (tests/resource_usage.rs) and print the results as Markdown tables.
#
# Release build, one test thread, output shown. See the test's module docs for
# the knobs (SHOESTRING_MEASURE_SECS, SHOESTRING_MEASURE_RENDERER,
# SHOESTRING_MEASURE_JSON). Linux only: it reads /proc.
#
#   scripts/measure-headless.sh

set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

cargo build --release -p shoestring-wm -p shoestring-remote-server -p shoestring-lock

# Runtime dirs go under TMPDIR, and unix socket paths are capped near 108 bytes.
export TMPDIR=/tmp

cargo test --release -p shoestring-wm --test resource_usage -- \
    --ignored --nocapture --test-threads=1
