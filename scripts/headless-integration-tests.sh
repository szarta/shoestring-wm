#!/usr/bin/env bash
# Run the WM's #[ignore]'d integration tests against a real, headless
# shoestring-wm instance.
#
# Those tests connect to a *running* compositor over $WAYLAND_DISPLAY and are
# marked #[ignore] so a normal `cargo test` (and a developer's in-session run,
# where some tests would mutate their live desktop) skips them. This harness
# stands up a throwaway winit-backed WM nested in an X server and runs them
# with `--ignored`.
#
# The winit backend needs an X display + EGL. In CI that's Xvfb + Mesa's
# llvmpipe software renderer; wrap this script accordingly:
#
#   xvfb-run -a -s "-screen 0 1280x800x24" scripts/headless-integration-tests.sh
#
# Locally you can run it the same way, or with DISPLAY already pointing at any
# X server (a winit window will appear). It must NOT run against your live
# session's $WAYLAND_DISPLAY — it launches its own WM in an isolated
# XDG_RUNTIME_DIR and never touches the outer one.

set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

if [ -z "${DISPLAY:-}" ]; then
    echo "error: DISPLAY is unset — wrap this in xvfb-run, e.g." >&2
    echo "  xvfb-run -a -s \"-screen 0 1280x800x24\" $0" >&2
    exit 2
fi

target_dir="${CARGO_TARGET_DIR:-target}"
wm_bin="$target_dir/debug/shoestring-wm"

echo "==> building shoestring-wm (winit backend)"
cargo build -p shoestring-wm

# Isolated runtime dir so the nested WM's wayland + IPC sockets never collide
# with (or clobber) the caller's session. Software GL keeps it GPU-free in CI.
runtime_dir="$(mktemp -d "${TMPDIR:-/tmp}/sw-headless.XXXXXX")"
chmod 700 "$runtime_dir"
wm_log="$runtime_dir/wm.log"
export LIBGL_ALWAYS_SOFTWARE=1

# Minimal config: no autostart clients (the bar / applets / portal aren't built
# or installed in CI and only add noise + load to the test compositor).
config="$runtime_dir/config.toml"
cat >"$config" <<'TOML'
[general]
autostart = []
TOML

wm_pid=""
cleanup() {
    [ -n "$wm_pid" ] && kill "$wm_pid" 2>/dev/null || true
    rm -rf "$runtime_dir"
}
trap cleanup EXIT

echo "==> launching headless WM (runtime dir: $runtime_dir)"
env -u WAYLAND_DISPLAY \
    XDG_RUNTIME_DIR="$runtime_dir" \
    SHOESTRING_WM_LOG="$wm_log" \
    "$wm_bin" --backend winit --config "$config" >/dev/null 2>&1 &
wm_pid=$!

# Wait for the WM's wayland socket to appear (it creates wayland-N in the
# isolated runtime dir). Bail early if the WM process dies.
socket=""
for _ in $(seq 1 100); do
    socket="$(ls "$runtime_dir"/wayland-? 2>/dev/null | head -1 || true)"
    [ -n "$socket" ] && break
    if ! kill -0 "$wm_pid" 2>/dev/null; then
        echo "error: WM exited before opening a socket; log:" >&2
        cat "$wm_log" >&2 || true
        exit 1
    fi
    sleep 0.3
done

if [ -z "$socket" ]; then
    echo "error: timed out waiting for the WM's wayland socket; log:" >&2
    tail -40 "$wm_log" >&2 || true
    exit 1
fi

wayland_display="$(basename "$socket")"
echo "==> WM up on $wayland_display; running ignored integration tests"

# `--ignored` runs only the #[ignore]'d tests; `-p shoestring-wm` scopes to the
# WM's test binaries (the integration suites). Picks up any future ignored
# integration test automatically.
status=0
XDG_RUNTIME_DIR="$runtime_dir" WAYLAND_DISPLAY="$wayland_display" \
    cargo test -p shoestring-wm -- --ignored || status=$?

if [ "$status" -ne 0 ]; then
    echo "==> integration tests failed; WM log tail:" >&2
    tail -60 "$wm_log" >&2 || true
fi
exit "$status"
