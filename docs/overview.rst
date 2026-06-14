Overview
========

shoestring-wm is a floating Wayland window manager written in Rust on top
of the Smithay compositor toolkit. It is the daily-driver replacement for
the author's Openbox/X11 setup. The repository is a Cargo workspace that
ships the compositor alongside the small set of desktop pieces that
finish out the environment:

============================  ==========================================
**shoestring-wm**             The compositor: input, output, layout, IPC.
**shoestring-bar**            A status bar (workspaces, focused window,
                              clock, battery) — consumes the WM's IPC
                              stream.
**shoestring-menu**           A dmenu-style launcher for commands and
                              bookmarks; bound to ``Super+P`` /
                              ``Super+B`` by default.
**shoestring-notify**         Notification daemon
                              (``org.freedesktop.Notifications``);
                              renders pop-ups via layer-shell.
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

**Global workspaces.** Configurable workspace count (default 16, capped
at 16), shared across all monitors: switching the active workspace
swaps every monitor at once. Per-window monitor assignment is
preserved, so a window that lived on monitor B reappears on monitor B
when its workspace returns. Workspaces can be given sparse per-slot
names via ``[workspaces].names``.

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
- Gestures, tablet, accessibility.
- (XWayland was deferred until GIMP forced it; now shipped — see the
  "What ships" section.)

What ships in v1
----------------

Implemented today:

- Winit backend for development inside an existing X11/Wayland session.
- Native DRM/KMS + libinput + libseat (libudev) backend for TTY use,
  including ``Ctrl+Alt+F1..F12`` VT switching.
- Configurable global workspaces (default 16, sparse named slots),
  multi-monitor, hotplug-safe.
- Per-window floating geometry with TileLeft / TileRight / Maximize /
  Minimize and floating-rect save/restore.
- Per-app window rules (app_id / title-contains → workspace, position,
  size). For X11 toplevels the rule matcher reads ``WM_CLASS`` as the
  app_id equivalent.
- XWayland integration: X11 toplevels map alongside Wayland windows,
  with bidirectional clipboard and primary-selection forwarding. Spawn
  any X11 app (``gimp``, ``inkscape``, ``feh``) directly from a
  terminal once the compositor's running; ``$DISPLAY`` is exported on
  Xwayland Ready.
- Layer-shell + foreign-toplevel-list (so the bar, menu, locker and
  notification helpers can attach).
- wlr-foreign-toplevel-management (``zwlr_foreign_toplevel_management_v1``):
  the writable taskbar protocol — waybar-style bars can activate, close,
  minimize and maximize windows and read each window's state.
- Pointer lock / confinement (``zwp_pointer_constraints_v1``) with
  relative motion (``zwp_relative_pointer_v1``): FPS games and RDP/VNC
  clients can lock the cursor in place (receiving only relative deltas)
  or confine it to a region of their surface. A lock activates while the
  pointer is over the requesting surface and releases when it leaves.
- Idle management (opt-in via ``[general].idle_notifications_enabled``):
  ``ext_idle_notify_v1`` for idle daemons/auto-lockers, paired with
  ``zwp_idle_inhibit_manager_v1`` so video players and browsers can
  suppress idle while a *visible* surface requests it — an inhibitor on a
  minimized or off-workspace window is ignored.
- xcursor sprite rendering at the pointer.
- Configurable XKB keyboard layouts (``[general].xkb_layout`` and
  friends), with multiple layouts and a ``cycle-layout`` action
  (``Super+Space`` by default) to switch between them at runtime.
- IPC server with query + event-stream subscriptions, plus key/text/
  click injection, action dispatch, find-windows regex search, command
  execution, and screenshot capture (the last four gated by a runtime
  automation gate).
- HiDPI / output-scale handling (integer and fractional).
- wlr-screencopy + region picker (``shoestring-screenshot`` +
  ``shoestring-region``).
- ``ext-session-lock-v1`` + PAM unlock via ``shoestring-lock``, with an
  xscreensaver-style maze-2d screensaver rendered behind the prompt.
- TOML config hot-reload via filesystem watcher (and an explicit
  ``reload-config`` action / IPC trigger).
- Configurable autostart list.
- ``--write-default-config`` to bootstrap a fresh user config.

See :doc:`architecture` for the source-level breakdown.
