#!/usr/bin/env bash
# Run the end-to-end tests for the native headless backend and remote sessions
# (tests/e2e_headless.rs).
#
# Each test starts its own `shoestring-wm --backend headless` with Mesa's
# software renderer (llvmpipe) in a private runtime dir, so no GPU, X server,
# or running session is needed, and nothing touches the caller's session. The
# remote tests also run the real `shoestring-remote-server`, which is built
# here because cargo only builds the test's own package.
#
#   scripts/headless-e2e-tests.sh [extra cargo test args, e.g. a test name]

set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

echo "==> building shoestring-wm and shoestring-remote-server"
cargo build -p shoestring-wm -p shoestring-remote-server

# The tests make their runtime dirs under TMPDIR, and unix socket paths are
# capped near 108 bytes, so don't inherit a deep TMPDIR.
export TMPDIR=/tmp

echo "==> running e2e tests"
cargo test -p shoestring-wm --test e2e_headless -- --ignored "$@"

# Again with `[headless] idle_suspend` on in every test, so a parked render
# loop is held to the same behaviour as a ticking one.
echo "==> running e2e tests with idle_suspend"
SHOESTRING_E2E_IDLE_SUSPEND=1 cargo test -p shoestring-wm --test e2e_headless -- --ignored "$@"
