#!/bin/sh
# Test an application end to end on an isolated, headless shoestring-wm.
#
# The application here is zenity's entry dialog: the test types a name,
# presses Return, and checks what zenity printed. Swap in your own app,
# window match, input and checks; the scaffolding stays the same.
#
# Needs shoestring-wm, shoestring-ctl, jq, and (for this example) zenity.
# Set KEEP=1 to keep the run directory after a pass as well.
set -eu

# --- An isolated compositor -------------------------------------------------

# Everything lives in one private directory: the WM's Wayland and IPC
# sockets, the app's runtime files, logs, and artifacts. Unix socket paths
# are capped near 108 bytes, so keep it short.
run=$(mktemp -d /tmp/apptest.XXXXXX)
chmod 700 "$run"
mkdir "$run/artifacts"
wm_pid=
app_pid=

# A config that changes only what the test depends on: nothing autostarted
# (no bar, no notification daemon), and a known background colour.
cat >"$run/config.toml" <<'EOF'
[general]
autostart = []

[background]
color = "#102030"
EOF

ctl() { shoestring-ctl "$@"; }

# Poll a command until it succeeds, for at most $1 seconds. For conditions
# the WM can't wait on itself; window maps and unmaps use wait-window.
wait_until() {
    limit=$1
    shift
    tries=0
    until "$@"; do
        tries=$((tries + 1))
        [ "$tries" -lt $((limit * 20)) ] || return 1
        sleep 0.05
    done
}

# On failure, save what explains it before teardown removes it.
collect() {
    ctl screenshot --path "$run/artifacts/failure.png" >/dev/null 2>&1 || true
    ctl -p tree >"$run/artifacts/tree.json" 2>&1 || true
    ctl -p server-info >"$run/artifacts/server-info.json" 2>&1 || true
    cp "$run/app.out" "$run/app.err" "$run/wm.log" "$run/wm.stderr" \
        "$run/artifacts/" 2>/dev/null || true
}

finish() {
    status=$?
    trap - EXIT INT TERM
    [ "$status" -eq 0 ] || collect
    # The app is our child, not the WM's: stop it ourselves.
    [ -z "$app_pid" ] || kill "$app_pid" 2>/dev/null || true
    if [ -n "$wm_pid" ]; then
        # shutdown stops the WM and its own children and removes its sockets.
        ctl shutdown >/dev/null 2>&1 || kill "$wm_pid" 2>/dev/null || true
        wait "$wm_pid" 2>/dev/null || true
    fi
    if [ "$status" -eq 0 ] && [ -z "${KEEP:-}" ]; then
        rm -rf "$run"
        echo "PASS"
    else
        [ "$status" -eq 0 ] && echo "PASS" || echo "FAIL (exit $status)"
        echo "run directory kept: $run"
    fi
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT TERM

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# The headless backend with the CPU renderer, a fixed output size (so
# coordinates mean the same thing on every run) and the automation gate on.
# The variables of the session running this script must not leak in.
env -u WAYLAND_DISPLAY -u DISPLAY -u SHOESTRING_WM_SOCKET \
    XDG_RUNTIME_DIR="$run" SHOESTRING_WM_LOG="$run/wm.log" \
    shoestring-wm --backend headless --renderer pixman \
    --output-size 1280x800 --enable-automation \
    --config "$run/config.toml" 2>"$run/wm.stderr" &
wm_pid=$!

# --- Readiness ----------------------------------------------------------------

# The IPC socket is named after the Wayland socket and appears after it.
ipc_socket() {
    kill -0 "$wm_pid" 2>/dev/null || fail "the WM exited during startup"
    for s in "$run"/shoestring-wm-*.sock; do
        [ -S "$s" ] && return 0
    done
    return 1
}
wait_until 10 ipc_socket || fail "no IPC socket"
set -- "$run"/shoestring-wm-*.sock
SHOESTRING_WM_SOCKET=$1
export SHOESTRING_WM_SOCKET
display=$(basename "$SHOESTRING_WM_SOCKET" .sock)
display=${display#shoestring-wm-}

# Returns once the event loop answers. An X11 app would pass --xwayland,
# which waits for XWayland and $DISPLAY.
ctl wait-ready >/dev/null

# --- Launch the app and wait for its window ---------------------------------

XDG_RUNTIME_DIR="$run" WAYLAND_DISPLAY="$display" GDK_BACKEND=wayland \
    zenity --entry --title=Rename --text="New name:" \
    >"$run/app.out" 2>"$run/app.err" &
app_pid=$!

# Blocks until the window has its first frame and its place on the output.
# Exits 2 on timeout.
window=$(ctl wait-window --app-id '^zenity$' --title '^Rename$' --timeout 10000) ||
    fail "the dialog never appeared"
geometry=$(echo "$window" | jq -r '.window.geometry | "\(.x),\(.y),\(.w),\(.h)"')

# Park the pointer in the bottom-right corner: the arrow points down and
# right from there, off the output, so it never shows up in a pixel probe,
# a hash or a screenshot. (Left at the top-left, it covers that corner.)
ctl move-mouse 1279 799 >/dev/null

# --- Drive it and check the result ------------------------------------------

# A pixel probe: outside the dialog is the configured background, so no
# other window or panel is in the way.
pixel=$(ctl get-pixel 5 5 | jq -c '[.r, .g, .b]')
[ "$pixel" = "[16,32,48]" ] || fail "unexpected background $pixel"

# The dialog focuses its text entry, so typing goes there. Wait for the
# typing to show by hashing the window's region: a changed hash means a
# repaint, with no screenshot to decode and no fixed sleep.
region_hash() { ctl hash-region --region "$geometry" | jq -r .hash; }
before=$(region_hash)
ctl type "Ada Lovelace" >/dev/null
repainted() { [ "$(region_hash)" != "$before" ]; }
wait_until 5 repainted || fail "the dialog did not repaint after typing"

# A reference image of the window alone, even if something overlapped it.
id=$(echo "$window" | jq -r .window.id)
ctl screenshot --window "$id" --path "$run/artifacts/typed.png" >/dev/null

ctl key Return >/dev/null
ctl wait-window --title '^Rename$' --unmap --timeout 5000 >/dev/null ||
    fail "the dialog did not close"
wait "$app_pid" || fail "zenity exited with status $?"
app_pid=

answer=$(cat "$run/app.out")
[ "$answer" = "Ada Lovelace" ] || fail "zenity printed '$answer'"
