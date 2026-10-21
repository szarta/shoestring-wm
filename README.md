# shoestring-wm

A lightweight, low-dependency Wayland window manager written in Rust on
top of [Smithay](https://github.com/Smithay/smithay). A replacement for an
Openbox/X11 desktop with floating windows, half-tile snaps,
configurable global workspaces (default 16), multi-monitor, TOML config,
and JSON IPC. The bar and desktop helpers run as separate processes.

## In-repo binaries

The workspace ships the WM plus every helper and desktop sibling it
spawns. The helpers live in `crates/`; `cargo build --workspace`
produces the desktop tools below, the portal, and the remote-desktop tools.

| Binary | Purpose |
|---|---|
| `shoestring-wm` | The window manager itself (TTY session, nested winit, and native headless backends). |
| `shoestring-bar` | Status bar (workspaces, focused window, clock, battery; consumes WM IPC). |
| `shoestring-menu` | dmenu-style launcher for commands and bookmarks. |
| `shoestring-notify` | `org.freedesktop.Notifications` daemon (layer-shell pop-ups). |
| `shoestring-ctl` | Command-line IPC client (query state, fire actions, subscribe to events). |
| `shoestring-tasks` | Console (TUI) window/workspace manager - focus, move, rename, minimize/maximize, kill, screenshot any window over IPC. |
| `shoestring-mediad` | Media-session monitor for the bar. |
| `xdg-desktop-portal-shoestring` | ScreenCast and Screenshot portal backend. |
| `shoestring-clipboard` | Clipboard helper. |
| `shoestring-lock` | Session locker (`ext-session-lock-v1`), PAM-authenticated, maze screensaver. |
| `shoestring-screenshot` | PNG capture via wlr-screencopy; optional region via `shoestring-region`. |
| `shoestring-region` | Slurp-equivalent rectangle picker. |
| `shoestring-kill` | xkill-equivalent click-to-close picker. |
| `shoestring-confirm` | Modal yes/no helper (used by `Quit`, reusable for other destructive actions). |
| `shoestring-remote-server` | Remote-desktop server: serves this session to a viewer over a forwarded Unix socket (see `docs/remote.rst`). |
| `shoestring-remote-client` | Remote-desktop viewer: shows a remote session, or grafts one remote window, on the local machine axis. |

## Quick start

```sh
# Install build deps (Debian/Ubuntu; see docs/install.rst for others)
sudo apt install build-essential pkg-config \
    libwayland-dev libxkbcommon-dev \
    libdrm-dev libgbm-dev libegl-dev \
    libudev-dev libinput-dev libseat-dev libdisplay-info-dev \
    libpam0g-dev libpipewire-0.3-dev libclang-dev

# Optional: XWayland support at runtime (only if you run X11 tools such as
# GIMP or Inkscape). Install the Xwayland binary; the WM spawns it on demand.
#   sudo apt install xwayland

# Build (release; all workspace binaries land in target/release/).
# `--workspace` rebuilds the helper crates too, not just the WM.
#
# NOTE: `cargo install --path .` is NOT enough — it installs only the WM
# binary and silently skips the workspace helpers (shoestring-confirm,
# -bar, -mediad, ...). A missing shoestring-confirm makes confirm-gated
# actions (Log out / Shut down from the bar menu, Super+Shift+Q) no-op
# with only a log line. Use the build + install below, or
# `cargo install --path . --path crates/shoestring-confirm ...` per helper.
cargo build --release --workspace

# Drop them on $PATH — the WM and every helper / sibling it spawns
# (the bar, mediad, and the portal are autostarted; the rest are
# launched on demand by keybinds and IPC).
install -Dm755 -t ~/.local/bin/ \
  target/release/shoestring-{wm,bar,menu,notify,ctl,lock,screenshot,region,kill,confirm,mediad} \
  target/release/shoestring-{tasks,clipboard,remote-server,remote-client} \
  target/release/xdg-desktop-portal-shoestring

# Screen sharing (Zoom, browsers): the portal *binary* above is not enough on
# its own — the xdg-desktop-portal frontend also needs the routing config and
# the *.portal descriptor, or it has no ScreenCast/Screenshot backend and screen
# sharing silently hangs. Install both (the descriptor MUST live in a system
# data dir; ~/.local/share is not scanned for *.portal files):
install -Dm644 resources/shoestring-wm-portals.conf \
  ~/.config/xdg-desktop-portal/shoestring-wm-portals.conf
sudo install -Dm644 resources/shoestring.portal \
  /usr/share/xdg-desktop-portal/portals/shoestring.portal
# Then: systemctl --user restart xdg-desktop-portal
# Full details (D-Bus activation, multi-monitor chooser): docs/portals.rst.
# This replaces xdg-desktop-portal-wlr — do NOT install xdpw.

# Bootstrap a config at ~/.config/shoestring-wm/config.toml
shoestring-wm --write-default-config

# Try it nested inside your current session (backend auto-detects winit
# when WAYLAND_DISPLAY/DISPLAY is set)
runtime_dir=$(mktemp -d)
XDG_RUNTIME_DIR="$runtime_dir" shoestring-wm --command alacritty
rmdir "$runtime_dir"

# Or run natively from a TTY
shoestring-wm
```

Winit-only dev build (skips DRM / udev / libinput / libseat):

```sh
cargo build --release --no-default-features --features winit
```

## Default bindings

| Bind | Action |
|---|---|
| `Super+Return` | Spawn terminal |
| `Super+P` / `Super+B` | Command launcher / bookmarks (shoestring-menu) |
| `Super+E` / `Super+W` | Tile focused window left / right half |
| `Super+M` | Maximize |
| `Super+G` / `+Shift` / `+Ctrl` | Arrange workspace: grid / spiral / BSP (one-shot) |
| `Super+D` / `Super+Shift+D` | Minimize / unminimize last |
| `Super+X` | Close focused window |
| `Alt+Tab` / `Super+Down` | Cycle focus through the workspace's windows |
| `Super+H` / `Super+L` | Previous / next workspace (auto-repeat on hold) |
| `Super+Ctrl+H` / `Super+Ctrl+L` | Move focused window to previous / next workspace |
| `Super+1..9` | Focus workspace N |
| `Super+Shift+1..9` | Move focused window to workspace N |
| `Super+Shift+L` | Lock session |
| `Super+Shift+Q` | Quit (confirmation dialog) |
| `Ctrl+Alt+F1..F12` | Switch VT (TTY backend) |
| `XF86Audio*` / `XF86MonBrightness*` | Action scripts under `scripts/actions/` |
| `Super+drag` | Move window; drag across screen edge to shift workspaces |

## Documentation

The full user guide lives under [`docs/`](docs/) and builds with Sphinx:

```sh
python3 -m pip install sphinx
make -C docs html         # docs/_build/html/index.html
make -C docs man          # docs/_build/man/

# Aurora prose checks and setup: see CONTRIBUTING.md.
make -C docs lint
```

- [Overview](docs/overview.rst) - what it is and why.
- [Install](docs/install.rst) - per-distro deps (Debian, Fedora, Arch,
  Alpine, FreeBSD, NixOS).
- [Running](docs/running.rst) - winit vs. TTY backend, env vars, autostart.
- [Configuration](docs/configuration.rst) - every `[general]` field and
  every action type.
- [Default bindings](docs/bindings.rst) - full keymap reference.
- [IPC](docs/ipc.rst) - protocol, types, and example clients.
- [Portals](docs/portals.rst) - screen sharing (Zoom/browsers), the native
  ScreenCast/Screenshot backend, and backend-routing setup.
- [Architecture](docs/architecture.md) - source-level design notes.
- [Portability](docs/PORTABILITY.md) - Linux-ism audit + FreeBSD status.

## Status

Working today:

- Winit backend (nested dev) and native DRM/KMS + libinput + libseat backend.
- Configurable workspace count (default 16) with sparse per-slot names.
- Multi-monitor with hotplug; HiDPI / fractional scale.
- TileLeft / TileRight / Maximize / Minimize with floating-rect restore.
- One-shot workspace arrange into grid / spiral / BSP-dwindle (per output).
- Super+drag move; sustained edge-drag shifts the window across workspaces.
- Configurable focus model: click-to-focus (default), focus-follows-mouse, sloppy.
- XWayland integration: X11 toplevels (e.g. GIMP, Inkscape) map alongside Wayland windows; class/title window rules apply; clipboard + primary selection bridged.
- Layer-shell + foreign-toplevel-list (bar/menu/lock/notification enablement).
- wlr-foreign-toplevel-management (`zwlr_foreign_toplevel_management_v1`): waybar-style taskbars can activate / close / minimize / maximize windows and read their state.
- xcursor sprite rendering.
- Per-app window rules (app\_id / title → workspace, position, size).
- Config hot-reload via `notify` watcher (and `shoestring-ctl reload-config`).
- Configurable autostart list.
- `ext-session-lock-v1` + PAM unlock via `shoestring-lock`, with an
  xscreensaver-style maze-2d screensaver underneath the prompt.
- wlr-screencopy + region picker + IPC `screenshot` request.
- `zwlr_gamma_control_v1` (night-light: gammastep/wlsunset) plus IPC
  `set_gamma`/`reset_gamma` for color-temperature control without a
  client.
- IPC server: queries, event stream, `inject_key`/`text`/`click`,
  `move_mouse` + `pointer_position`,
  `focus_window`, `pick_window` + `close_window`, `find_windows`
  (title/app_id regex filter), `dispatch_action` (fire any keybind
  server-side), `run_command`, `screenshot`, `set_gamma`/`reset_gamma`,
  `set_automation` gate, hot-reload trigger.
- Modal confirm dialog primitive (`shoestring-confirm`); wraps `Quit` today.
- Action helper scripts (volume, brightness, logout) wired to XF86 keys.

## Roadmap

Notable open items:

- Server-side decoration rendering (border + optional titlebar).
- FreeBSD smoke-test of the winit build.
- BSD branches for the `shoestring-brightness-*` helper scripts.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to build, run, and test the
project, and [`docs/architecture.md`](docs/architecture.md) for the design
and the reasoning behind it. Commit rules for people and agents alike (hooks, DCO
sign-off, attribution) are in [AGENTS.md](AGENTS.md).

## AI Assistance

shoestring-wm was developed with assistance from [Claude](https://claude.ai) (Anthropic), used for code generation, bug fixes, release packaging, and documentation.

Many earlier commits have a `Co-Authored-By: Claude` trailer, but not every AI-assisted commit does, so the trailers are not a complete record. From October 2026 the contribution is acknowledged once, here, and not with a co-author trailer on each commit.

## License

MIT - see [LICENSE](LICENSE).
