shoestring-tasks
================

Synopsis
--------

| **shoestring-tasks** [**-s**\|\ **--socket** *PATH*]

Description
-----------

A console (TUI) "mission control" for **shoestring-wm**\(1). It lists every
window grouped by workspace — including windows on inactive workspaces and
minimized windows — and drives the WM's per-window IPC so you can manage any
window without keybind gymnastics: focus, close, force-kill, rename, move to
another workspace, minimize/maximize, pin sticky or always-on-top, raise or
lower, and screenshot a single window.

It is a thin client over the WM's IPC surface (the same wire protocol used by
**shoestring-ctl**\(1)); it links neither the compositor nor Smithay. The view
stays live by subscribing to the WM event stream and re-fetching its snapshot
whenever the WM's state changes — no polling.

Run it in any terminal, or bind a key to spawn one in your terminal emulator.

Options
-------

**-s**, **--socket** *PATH*
    Override the IPC socket path. By default the socket is resolved from
    ``$SHOESTRING_WM_SOCKET``, falling back to
    ``$XDG_RUNTIME_DIR/shoestring-wm-$WAYLAND_DISPLAY.sock``.

**-h**, **--help**
    Print usage and exit.

**-V**, **--version**
    Print the version and exit.

Key bindings
------------

Navigation is vim-style; commands act on the selected window. Press ``?``
inside the UI for this list.

**j** / **k**, **Down** / **Up**
    Move the selection between windows (workspace headers are skipped).

**g** / **G**
    Jump to the first / last window.

**Enter**
    Focus the window (switches to its workspace, unminimizing if needed).

**x**
    Close the window politely (``xdg_toplevel.close``).

**X**
    Force-kill the window (SIGKILL the owning process); asks to confirm.

**r**
    Rename the window — sets a display-name override. An empty name clears it.

**m** / **M**
    Toggle minimize / maximize.

**t** / **a**
    Toggle sticky (show on all workspaces) / always-on-top.

**R** / **L**
    Raise / lower the window in the stacking order.

**s**
    Screenshot the selected window. Requires the screen-capture gate to be on
    (enable it with ``shoestring-ctl screen-capture on`` or
    ``shoestring-ctl automation on``). Only works for a window on the active,
    visible workspace.

**1**–**9**, **0**
    Move the window to that workspace (``0`` = workspace 10).

**w** / **>**
    Move the window to a workspace by number (prompts; reaches 1–16).

**z**
    Show or hide empty, inactive workspaces.

**F5**
    Refresh the snapshot now.

**q**, **Esc**, **Ctrl-C**
    Quit.

Environment
-----------

**SHOESTRING_WM_SOCKET**
    Path to the WM control socket. Used when ``--socket`` is not given.

**XDG_RUNTIME_DIR**, **WAYLAND_DISPLAY**
    Used to derive the default socket path when ``$SHOESTRING_WM_SOCKET`` is
    unset.

Exit status
-----------

**0**
    Normal exit.

**2**
    Bad arguments, or the socket path could not be resolved.

See also
--------

**shoestring-wm**\(1), **shoestring-ctl**\(1), **shoestring-kill**\(1)
