#!/bin/sh
# Entrypoint for the headless serve-mode compositor container.
#
# Boots shoestring-wm (headless), waits for its IPC socket, lets the
# WM-autostarted shoestring-remote-server register, opens the remote gate, and
# (optionally) launches one app. Runs as PID 1 and forwards termination to the
# WM so `docker stop` / `podman stop` shuts the desktop down cleanly.
set -eu

log() { echo "[entrypoint] $*"; }

# Short, user-owned XDG_RUNTIME_DIR — unix socket paths must stay under ~108
# bytes (SUN_LEN), so a deep default like /home/<user>/.cache/... would break
# the wayland + IPC sockets. /run/shoestring is created in the image.
: "${XDG_RUNTIME_DIR:=/run/shoestring}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

# The remote bridge listens on a Unix socket (the image sets
# SHOESTRING_REMOTE_SOCKET) in a directory the host bind-mounts, and accepts
# only this container user's uid. Make sure the directory exists and is private
# even when nothing is mounted over it. SHOESTRING_REMOTE_TCP=ADDR:PORT opts
# into TCP instead, which checks nobody: see docs/containers.rst.
if [ -n "${SHOESTRING_REMOTE_TCP:-}" ]; then
    # The image's socket default would conflict with an explicit TCP choice.
    unset SHOESTRING_REMOTE_SOCKET
    # The host's -p can only reach a port bound beyond the container's own
    # loopback, and the server refuses such an address without a second
    # opt-in. Stop here with the fix rather than leave the WM running with no
    # remote server.
    case "$SHOESTRING_REMOTE_TCP" in
        127.*:* | localhost:* | "[::1]":*) ;;
        *)
            if [ "${SHOESTRING_REMOTE_TCP_ANY_HOST:-0}" != 1 ]; then
                log "ERROR: SHOESTRING_REMOTE_TCP=$SHOESTRING_REMOTE_TCP is not a loopback address, so the remote server refuses it without SHOESTRING_REMOTE_TCP_ANY_HOST=1."
                log "Set it only when the host publishes the port to loopback (-p 127.0.0.1:PORT:PORT) or every host that can reach the port is trusted. See docs/containers.rst."
                exit 1
            fi
            ;;
    esac
    log "WARN: remote server on TCP $SHOESTRING_REMOTE_TCP: nothing checks who connects"
elif [ -n "${SHOESTRING_REMOTE_SOCKET:-}" ]; then
    REMOTE_DIR=$(dirname "$SHOESTRING_REMOTE_SOCKET")
    mkdir -p "$REMOTE_DIR"
    # A bind mount owned by someone else means the host user isn't mapped to
    # this container's user, so the host couldn't use the socket anyway. Say
    # so, rather than fail on chmod with no hint.
    if [ ! -O "$REMOTE_DIR" ]; then
        log "ERROR: $REMOTE_DIR is owned by uid $(stat -c %u "$REMOTE_DIR"), not this container's user (uid $(id -u)), so the remote socket can't be made private to you."
        log "Map your host user to uid $(id -u): with podman, add --userns=keep-id:uid=$(id -u),gid=$(id -g); with docker, run as host uid $(id -u). See docs/containers.rst."
        exit 1
    fi
    chmod 700 "$REMOTE_DIR"
fi
if [ -n "${SHOESTRING_REMOTE_BIND:-}${SHOESTRING_REMOTE_PORT:-}" ]; then
    log "WARN: SHOESTRING_REMOTE_BIND/PORT are no longer supported (the remote server will refuse to start); use SHOESTRING_REMOTE_TCP=ADDR:PORT"
fi

CONFIG="${SHOESTRING_WM_CONFIG:-/etc/shoestring-wm/config.toml}"

log "starting shoestring-wm (headless), config=$CONFIG"
shoestring-wm --backend headless -c "$CONFIG" &
WM_PID=$!

# Forward stop signals to the WM and exit with its status.
trap 'kill "$WM_PID" 2>/dev/null || true' INT TERM

# Find the IPC socket the WM binds: $XDG_RUNTIME_DIR/shoestring-wm-<display>.sock
SOCK=""
i=0
while [ "$i" -lt 100 ]; do
    SOCK=$(ls "$XDG_RUNTIME_DIR"/shoestring-wm-*.sock 2>/dev/null | head -1 || true)
    [ -n "$SOCK" ] && [ -S "$SOCK" ] && break
    if ! kill -0 "$WM_PID" 2>/dev/null; then
        log "WM exited before its IPC socket appeared"
        wait "$WM_PID"; exit 1
    fi
    i=$((i + 1)); sleep 0.1
done
[ -S "$SOCK" ] || { log "timed out waiting for WM IPC socket"; kill "$WM_PID" 2>/dev/null || true; exit 1; }
export SHOESTRING_WM_SOCKET="$SOCK"
log "WM IPC at $SOCK"

# WAYLAND_DISPLAY for any app we launch below (the WM sets this for its own
# children, but we are the WM's parent so we derive it from the socket name).
WLD=$(basename "$SOCK" .sock); WLD="${WLD#shoestring-wm-}"
export WAYLAND_DISPLAY="$WLD"

# Wait for shoestring-remote-server (WM autostart) to register, then open the
# remote gate. Opt out with SHOESTRING_REMOTE_ENABLE=0 to start gated-off and
# flip it later (`podman exec ... shoestring-ctl remote on`).
if [ "${SHOESTRING_REMOTE_ENABLE:-1}" = "1" ]; then
    i=0
    while [ "$i" -lt 100 ]; do
        if shoestring-ctl -s "$SOCK" remote status 2>/dev/null | grep -q '"server_available":true'; then
            break
        fi
        i=$((i + 1)); sleep 0.1
    done
    log "opening remote gate"
    shoestring-ctl -s "$SOCK" remote on \
        || log "WARN: could not open remote gate (remote-server not registered?)"
fi

# Optional one-app-per-container: the true per-app-isolation pattern. Pass the
# app's argv in $SHOESTRING_APP (e.g. -e SHOESTRING_APP='alacritty'). It runs
# against this WM via WAYLAND_DISPLAY; detached so it does not block shutdown.
if [ -n "${SHOESTRING_APP:-}" ]; then
    log "launching app: $SHOESTRING_APP"
    setsid sh -c "$SHOESTRING_APP" >/dev/null 2>&1 &
fi

log "ready — desktop is up; view it with shoestring-remote-client"
wait "$WM_PID"
