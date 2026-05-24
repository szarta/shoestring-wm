# shoestring-wm

A lightweight, low-dependency Wayland window manager written in Rust on
top of [Smithay](https://github.com/Smithay/smithay). It is a daily-driver
replacement for an Openbox/X11 setup: floating windows, half-tile snaps,
16 global workspaces, multi-monitor, TOML config, JSON IPC. No
animations, no decoration polish, no built-in bar.

Sibling projects round out the desktop:

- **[shoestring-bar](https://github.com/szarta/shoestring-bar)** — status
  bar (workspaces, focused window, clock).
- **[shoestring-menu](https://github.com/szarta/shoestring-menu)** —
  dmenu-style launcher for commands and bookmarks.

## Quick start

```sh
# Install build deps (Debian/Ubuntu; see docs for other distros)
sudo apt install build-essential pkg-config \
    libwayland-dev libxkbcommon-dev \
    libdrm-dev libgbm-dev libegl-dev \
    libudev-dev libinput-dev libseat-dev libdisplay-info-dev

# Build & install
cargo install --path .

# Bootstrap a config
shoestring-wm --write-default-config

# Try it inside your current session (winit backend)
shoestring-wm --command alacritty

# Or run natively from a TTY
shoestring-wm
```

Defaults: `Super+E/W` half-tile, `Super+M` maximize, `Super+D` minimize,
`Super+1..9` workspaces, `Super+Return` terminal, `Super+P` launcher,
`Super+Shift+Q` quit.

## Documentation

The full user guide lives under [`docs/`](docs/) and builds with Sphinx:

```sh
cd docs && make html      # _build/html/index.html
cd docs && make man       # _build/man/shoestring-wm.{1,5}, etc.
```

- [Overview](docs/overview.rst) — what it is and why.
- [Install](docs/install.rst) — per-distro deps for Debian, Fedora,
  Arch, Alpine, FreeBSD, NixOS.
- [Running](docs/running.rst) — winit vs. TTY backend, env vars, autostart.
- [Configuration](docs/configuration.rst) — every `[general]` field and
  every action type.
- [Default bindings](docs/bindings.rst) — the keymap shipped by
  `--write-default-config`.
- [IPC](docs/ipc.rst) — protocol, types, and example clients.
- [Architecture](docs/architecture.md) — source-level design notes.

## Status

Implemented today (v1):

- Winit backend (nested dev) and native DRM/KMS + libinput + libseat backend.
- 16 global workspaces; multi-monitor with hotplug; HiDPI / fractional scale.
- TileLeft / TileRight / Maximize / Minimize with floating-rect restore.
- Layer-shell + foreign-toplevel-list (bar/menu enablement).
- xcursor sprite rendering; Super+drag move/resize.
- IPC server with query + event-stream subscriptions.
- `--write-default-config` for a turnkey starter config.

Roadmap items live in `todo.sqlite` and include per-app window rules,
config hot-reload, focus-follows-mouse, XWayland integration, and a
configurable autostart list.

## License

MIT — see [LICENSE](LICENSE).
