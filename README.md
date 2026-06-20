# shoestring-wm

A lightweight, low-dependency Wayland window manager written in Rust on
top of [Smithay](https://github.com/Smithay/smithay). Daily-driver
replacement for an Openbox/X11 setup: floating windows, half-tile snaps,
configurable global workspaces (default 16), multi-monitor, TOML config,
JSON IPC. No animations, no decoration polish, no built-in bar.

## In-repo binaries

The workspace ships the WM plus every helper and desktop sibling it
spawns. As of the monorepo migration, bar/menu/notify live in
`crates/` alongside the original helpers — one `cargo build
--workspace` produces everything below.

| Binary | Purpose |
|---|---|
| `shoestring-wm` | The window manager itself (winit + TTY backends). |
| `shoestring-bar` | Status bar (workspaces, focused window, clock, battery; consumes WM IPC). |
| `shoestring-menu` | dmenu-style launcher for commands and bookmarks. |
| `shoestring-notify` | `org.freedesktop.Notifications` daemon (layer-shell pop-ups). |
| `shoestring-ctl` | Command-line IPC client (query state, fire actions, subscribe to events). |
| `shoestring-lock` | Session locker (`ext-session-lock-v1`), PAM-authenticated, maze screensaver. |
| `shoestring-screenshot` | PNG capture via wlr-screencopy; optional region via `shoestring-region`. |
| `shoestring-region` | Slurp-equivalent rectangle picker. |
| `shoestring-kill` | xkill-equivalent click-to-close picker. |
| `shoestring-confirm` | Modal yes/no helper (used by `Quit`, reusable for other destructive actions). |

## Quick start

```sh
# Install build deps (Debian/Ubuntu; see docs/install.rst for others)
sudo apt install build-essential pkg-config \
    libwayland-dev libxkbcommon-dev \
    libdrm-dev libgbm-dev libegl-dev \
    libudev-dev libinput-dev libseat-dev libdisplay-info-dev \
    libpam0g-dev

# Optional: XWayland support at runtime (only if you run X11 tools such as
# GIMP or Inkscape). Install the Xwayland binary; the WM spawns it on demand.
#   sudo apt install xwayland

# Build (release; all workspace binaries land in target/release/).
# `--workspace` rebuilds the helper crates too, not just the WM.
cargo build --release --workspace

# Drop them on $PATH — the WM and every helper / sibling it spawns.
install -Dm755 -t ~/.local/bin/ \
  target/release/shoestring-{wm,bar,menu,notify,ctl,lock,screenshot,region,kill,confirm}

# Bootstrap a config at ~/.config/shoestring-wm/config.toml
shoestring-wm --write-default-config

# Try it nested inside your current session (backend auto-detects winit
# when WAYLAND_DISPLAY/DISPLAY is set)
shoestring-wm --command alacritty

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
cd docs && make html      # _build/html/index.html
cd docs && make man       # _build/man/shoestring-wm.{1,5}, etc.
```

- [Overview](docs/overview.rst) — what it is and why.
- [Install](docs/install.rst) — per-distro deps (Debian, Fedora, Arch,
  Alpine, FreeBSD, NixOS).
- [Running](docs/running.rst) — winit vs. TTY backend, env vars, autostart.
- [Configuration](docs/configuration.rst) — every `[general]` field and
  every action type.
- [Default bindings](docs/bindings.rst) — full keymap reference.
- [IPC](docs/ipc.rst) — protocol, types, and example clients.
- [Architecture](docs/architecture.md) — source-level design notes.
- [Portability](docs/PORTABILITY.md) — Linux-ism audit + FreeBSD status.

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

Tracked in `todo.sqlite` (via `todo-sqlite-cli`). Notable open items:

- Server-side decoration rendering (border + optional titlebar).
- FreeBSD smoke-test of the winit build.
- BSD branches for the `shoestring-brightness-*` helper scripts.

## License

MIT — see [LICENSE](LICENSE).
