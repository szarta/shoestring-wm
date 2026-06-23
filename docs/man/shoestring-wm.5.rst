shoestring-wm (config)
======================

Synopsis
--------

``$XDG_CONFIG_HOME/shoestring-wm/config.toml``

Description
-----------

Configuration file for **shoestring-wm**\(1). TOML syntax; the main
sections (all optional):

::

    [general]
    focus_mode          = "click-to-focus"  # click-to-focus | follows-mouse | sloppy
    repeat_delay        = 600               # ms before key repeat kicks in
    repeat_rate         = 25                # repeats per second
    desktop_scroll_notches = 1              # wheel detents per desktop-scroll workspace switch
    output_scale        = 1.0               # wl_output.scale (whole or fractional)
    lock_command        = "shoestring-lock"
    autostart           = ["shoestring-bar"]
    automation_enabled  = false             # gate for remote-automation IPC

    [workspaces]
    count = 16                               # 1..=16
    names = { 1 = "main", 2 = "web" }       # sparse, 1-based

    [[bindings]]
    mods   = ["Super"]
    key    = "Return"
    action = { type = "spawn", command = "alacritty" }

    [[window_rules]]
    match   = { app_id = "firefox" }
    actions = { workspace = 2 }

    [decorations]
    border_width   = 0                       # 0 = no border (default)
    focused_color  = "#5e81ac"
    unfocused_color = "#4c566a"

Generate a starter file with ``shoestring-wm --write-default-config``.

[general] keys
--------------

**focus_mode** (string, default ``click-to-focus``)
    One of ``click-to-focus``, ``follows-mouse``, ``sloppy``.
    ``follows-mouse`` moves keyboard focus to whatever window the pointer
    is over, and clears focus over empty space. ``sloppy`` is the same
    but keeps the previous focus while the pointer is over empty space.
    Neither variant raises the window — clicks still do.

**repeat_delay** (integer, default ``600``)
    Milliseconds before a held key starts repeating.

**repeat_rate** (integer, default ``25``)
    Key-repeat rate in repeats per second.

**desktop_scroll_notches** (integer, default ``1``)
    Mouse-wheel detents required to switch one workspace when scrolling the
    bare desktop (no window or layer surface under the pointer). ``1``
    switches one workspace per physical notch; higher values slow it down.
    High-resolution wheels that emit several sub-detent events per notch are
    accumulated, so one notch never overshoots. Touchpad scrolling is
    unaffected. Treated as at least ``1``.

**output_scale** (float, default ``1.0``)
    Scale advertised on ``wl_output.scale``. Whole numbers send an
    integer scale; fractional values use ``wp_fractional_scale_v1``.

**lock_command** (string, default ``"shoestring-lock"``)
    Command spawned by the ``lock`` action and the ``lock`` IPC
    request. Whitespace-split into argv.

**autostart** (array of strings, default ``["shoestring-bar"]``)
    Commands spawned once at WM startup, after the wayland socket is
    listening. Each entry is whitespace-split into argv. Failures log
    a warning and do not block startup. Set to ``[]`` to disable.

**automation_enabled** (bool, default ``false``)
    Master gate for the remote-automation IPC methods (key/text/click
    injection, screenshot, run-command, dispatch-action). Overridable
    at runtime with ``shoestring-wm --enable-automation`` or
    ``shoestring-ctl automation on/off``.

[workspaces] keys
-----------------

**count** (integer, default ``16``)
    Number of workspaces. Valid range ``1..=16``.

**names** (table of string-keyed string, default ``{}``)
    Sparse map of 1-based workspace index → display name. Empty/missing
    slots render as the number. Indexes greater than ``count`` are
    rejected at parse time.

[[bindings]] entries
--------------------

**mods**
    Array of modifier names, case-insensitive:
    ``Super``, ``Ctrl``, ``Alt``, ``Shift``.

**key**
    xkb keysym name. Use the unshifted form for letters: ``"q"``,
    ``"Return"``, ``"F1"``, ``"space"``.

**action**
    Tagged enum ``{ type = "...", ... }``. Supported types:

    ``spawn``
        Fields: ``command`` (string), optional ``args`` (string array).

    ``quit``
        Bring up the modal confirm dialog (``shoestring-confirm``);
        exit on *Yes*, stay running on *No*.

    ``lock``
        Spawn ``[general].lock_command``.

    ``reload-config``
        Re-read the config file.

    ``tile-left`` / ``tile-right``
        Half-tile the focused window on its monitor (toggle).

    ``maximize``
        Maximize to the monitor's usable rect (toggle).

    ``arrange-grid`` / ``arrange-spiral`` / ``arrange-bsp``
        One-shot auto-arrange of every window on the active workspace
        (per output, in reading order) into a grid, fibonacci spiral, or
        binary "dwindle" split. No persistent tiling — the windows stay
        floating, so later opens/closes do not re-flow; invoke again to
        re-tile. Skips minimized and fullscreen windows.

    ``minimize``
        Hide the focused window.

    ``unminimize``
        Restore the most-recently-minimized window of the active
        workspace.

    ``close``
        Ask the client to close.

    ``cycle-windows``
        Focus and raise the next window on the active workspace (Alt+Tab),
        round-robining through all of them. No-op below two windows.

    ``focus-workspace``
        Field ``index`` (1..=16).

    ``focus-workspace-relative``
        Field ``delta`` (typically ``-1`` or ``1``); saturating.

    ``move-window-to-workspace``
        Field ``index`` (1..=16).

    ``move-window-to-workspace-relative``
        Field ``delta``; saturating.

    ``change-vt``
        Field ``vt`` (1..=12). TTY backend only.

    ``inject-key``
        Field ``keysym`` (string, xkb keysym name). Synthesizes a
        press+release into the focused surface, bypassing the WM's
        binding table.

    ``inject-text``
        Field ``text`` (string). ASCII letters, digits, and space
        only in v1.

    ``inject-click``
        Field ``button`` (``"left"`` / ``"right"`` / ``"middle"`` or a
        numeric ``BTN_*`` code). Clicks at the current pointer.

[[window_rules]] entries
------------------------

Per-app actions evaluated once on first commit after a window maps.

**match** (table)
    Sparse predicate. AND-ed; empty matcher matches every window.

    - ``app_id`` (string) — exact xdg-shell ``app_id`` match.
    - ``title_contains`` (string) — case-sensitive substring on title.

**actions** (table)
    Sparse action set.

    - ``workspace`` (1..=count) — assign to this workspace; user view
      does not follow.
    - ``position`` (``[x, y]``, logical px) — override the auto-centered
      spawn position.
    - ``size`` (``[w, h]``, logical px) — preferred size (client may
      negotiate).

Example::

    [[window_rules]]
    match = { app_id = "firefox" }
    actions = { workspace = 2 }

[portal] keys
-------------

Settings for the ``xdg-desktop-portal-shoestring`` screen-sharing backend
(read from this same file by that separate process).

**screencast_output** (string, unset by default)
    Pin screencast to one output by connector name (e.g. ``"DP-2"``); skips
    the chooser. Overridden by ``$SHOESTRING_SCREENCAST_OUTPUT``.

**screencast_chooser** (string, default ``region``)
    How to choose the shared output when none is pinned and more than one is
    connected: ``region`` (click the monitor via **shoestring-region**),
    ``none`` (use the first output), or a dmenu-style command (connector names
    on stdin, the chosen line on stdout). Overridden by
    ``$SHOESTRING_SCREENCAST_CHOOSER``.

[decorations] keys
------------------

Server-side window border. Off by default; a border is drawn only when
**border_width** is non-zero. The border is painted just inside each window's
own rectangle (so it never bleeds onto an adjacent tile) and is focus-aware.

**border_width** (integer, default ``0``)
    Border thickness in logical pixels. ``0`` disables the border. A window
    smaller than twice the width in either dimension is left undecorated.

**focused_color** (string, default ``#5e81ac``)
    Border color of the focused window, ``#RRGGBB`` or ``#RRGGBBAA`` hex. An
    unparseable value falls back to the default.

**unfocused_color** (string, default ``#4c566a``)
    Border color of unfocused windows, same format.

[debug] keys
------------

Runtime toggles for diagnosing the render path without a recompile. Each turns
off a DRM/KMS plane optimization so you can tell whether a plane is behind a
visual glitch. Honored **only on the DRM/KMS (TTY) backend** (the nested winit
backend has no hardware planes and ignores them), re-read every frame so
``reload-config`` applies a change on the next frame. Every flag defaults to
``false`` — leave the section out unless actively debugging.

**disable_direct_scanout** (bool, default ``false``)
    Force window content through GL composition instead of scanning an opaque
    or fullscreen surface out from a primary/overlay plane. Implies
    **disable_overlay_planes**.

**disable_overlay_planes** (bool, default ``false``)
    Disable only overlay-plane scanout, leaving primary-plane scanout in place.

**disable_cursor_plane** (bool, default ``false``)
    Composite the cursor into the frame instead of using a hardware cursor
    plane.

Examples
--------

Spawn a terminal::

    [[bindings]]
    mods = ["Super"]
    key = "Return"
    action = { type = "spawn", command = "alacritty" }

Snap left half::

    [[bindings]]
    mods = ["Super"]
    key = "e"
    action = { type = "tile-left" }

Move focused window to workspace 5::

    [[bindings]]
    mods = ["Super", "Shift"]
    key = "5"
    action = { type = "move-window-to-workspace", index = 5 }

Files
-----

``$XDG_CONFIG_HOME/shoestring-wm/config.toml``
    Default location.

``$XDG_CONFIG_HOME/shoestring-wm/executables`` and ``bookmarks``
    Companion files read by **shoestring-menu**\(1).

See also
--------

**shoestring-wm**\(1)
