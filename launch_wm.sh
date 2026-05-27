#!/usr/bin/env bash
# Launch shoestring-wm from a TTY with tracing redirected to a file.
#
# - Builds in --release (incremental; no-op if nothing's changed).
# - Selects the TTY backend explicitly (auto-detect would also pick it,
#   since a fresh TTY has neither $WAYLAND_DISPLAY nor $DISPLAY).
# - Sets SHOESTRING_WM_LOG so tracing goes to a file — stderr scrolls
#   past on the console while the compositor is rendering.
# - Defaults RUST_LOG=info; override by exporting it before invoking
#   (e.g. RUST_LOG=debug,smithay=info ./launch_wm.sh).
#
# Extra arguments are forwarded to shoestring-wm verbatim, e.g.:
#   ./launch_wm.sh --enable-automation
#   ./launch_wm.sh --config /path/to/config.toml
#
# Run from a real TTY (Ctrl-Alt-F2 etc.); the WM grabs the seat via
# libseat and won't share the GPU with an X/Wayland session.

set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
log_dir="${XDG_STATE_HOME:-$HOME/.local/state}/shoestring-wm"
mkdir -p "$log_dir"
log_file="$log_dir/wm.log"
# Catch-all for the WM's own stderr (EGL / wgpu chatter that bypasses
# tracing) and — more importantly — every spawned client's stderr,
# since children inherit our fds. shoestring-bar writes tracing to
# stderr with no log-file env of its own; without this capture its
# panics / errors vanish into the framebuffer.
stderr_file="$log_dir/stderr.log"

# --workspace rebuilds the helper crates too (shoestring-confirm,
# shoestring-ctl, shoestring-region, ...) so a launcher relaunch
# always picks up source changes to them, not just the WM. Without
# this, the stale binaries in target/release/ silently shadow your
# fixes via the PATH prepend below.
cargo build --release --workspace --manifest-path "$repo/Cargo.toml"

# Workspace helpers (shoestring-confirm, shoestring-screenshot,
# shoestring-region, shoestring-ctl, shoestring-kill) live in
# target/release/ but aren't typically installed to $PATH. Prepend
# the build dir so the WM finds them when spawning. ~/.cargo/bin
# wins for anything that's `cargo install`ed (e.g. shoestring-bar
# from the sibling repo).
export PATH="$repo/target/release:$PATH"

export SHOESTRING_WM_LOG="$log_file"
export RUST_LOG="${RUST_LOG:-info}"

echo "shoestring-wm: tracing -> $log_file" >&2
echo "shoestring-wm: stderr (incl. spawned children) -> $stderr_file" >&2
echo "  tail -f $log_file $stderr_file   (from another TTY / SSH session)" >&2
# Redirect stderr first, then exec. Children inherit fd 2 → file, so
# bar/menu/lock crash output lands in stderr.log too.
exec >>"$stderr_file" 2>&1
exec "$repo/target/release/shoestring-wm" --backend tty "$@"
