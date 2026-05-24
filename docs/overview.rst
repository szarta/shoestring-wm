Overview
========

shoestring-wm is a floating Wayland window manager written in Rust on top
of the Smithay compositor toolkit. It is the daily-driver replacement for
the author's Openbox/X11 setup, and it ships with two sibling tools that
finish out the desktop:

============================  ==========================================
**shoestring-wm**             The compositor: input, output, layout, IPC.
**shoestring-bar**            A status bar (workspaces, focused window,
                              clock) — consumes the WM's IPC stream.
**shoestring-menu**           A dmenu-style launcher for commands and
                              bookmarks; bound to ``Super+P`` /
                              ``Super+B`` by default.
============================  ==========================================

Design philosophy
-----------------

**Lightweight and low-dependency.** A native Wayland compositor cannot be
zero-dependency — it needs DRM/KMS, libinput, libseat, EGL — but every
non-essential crate is deliberately pushed back on. The intent is a
codebase a single person can hold in their head and an install footprint
small enough to fit on top of a base graphical Linux without dragging in
desktop-environment plumbing. The full dependency list is tracked in the
project's notes and reviewed when new crates land.

**Floating, not tiling.** No tree, no master/stack, no scrollable
workspaces. tmux already handles tiling-within-a-terminal. The window
operations are the four that the Openbox flow relies on:

- ``Super+E`` — snap to left half of the focused monitor.
- ``Super+W`` — snap to right half of the focused monitor.
- ``Super+M`` — maximize to the focused monitor's usable rect (toggle).
- ``Super+D`` — minimize. ``Super+Shift+D`` restores the most recent.

Plus ``Super+Left-drag`` to move and ``Super+Right-drag`` to resize, so
the pointer-driven Openbox feel carries straight over.

**Global workspaces.** 16 workspaces, shared across all monitors:
switching the active workspace swaps every monitor at once. Per-window
monitor assignment is preserved, so a window that lived on monitor B
reappears on monitor B when its workspace returns.

**TOML config, hot-reloadable.** Lives at
``$XDG_CONFIG_HOME/shoestring-wm/config.toml``. Every keybinding is
user-settable; the default bindings ship in the binary so a brand-new
user has a working keymap out of the box.

**Unix-socket JSON IPC.** Everything a bar, launcher, or scripts need —
workspace state, window list, focus changes — comes over a newline-JSON
unix socket exported as ``$SHOESTRING_WM_SOCKET``. See :doc:`ipc`.

Explicit non-goals (v1)
-----------------------

- Animations or fancy transitions.
- Server-side decoration polish.
- A built-in bar — shoestring-bar is intentionally a separate process.
- Fractional-scale gymnastics, gestures, tablet, screencopy.
- XWayland is deferred: the integration point exists in ``backend/``
  but the feature isn't wired in v1. It will be added when an app the
  author cares about forces it.

What ships in v1
----------------

Implemented today:

- Winit backend for development inside an existing X11/Wayland session.
- Native DRM/KMS + libinput + libseat (libudev) backend for TTY use,
  including ``Ctrl+Alt+F1..F12`` VT switching.
- 16 global workspaces, multi-monitor, hotplug-safe.
- Per-window floating geometry with TileLeft / TileRight / Maximize /
  Minimize and floating-rect save/restore.
- Layer-shell + foreign-toplevel-list (so the bar and menu can attach).
- xcursor sprite rendering at the pointer.
- IPC server with query + event-stream subscriptions.
- HiDPI / output-scale handling (integer and fractional).
- ``--write-default-config`` to bootstrap a fresh user config.

See :doc:`architecture` for the source-level breakdown.
