shoestring-wm (config)
======================

Synopsis
--------

``$XDG_CONFIG_HOME/shoestring-wm/config.toml``

Description
-----------

Configuration file for **shoestring-wm**\(1). TOML syntax, four
sections (all optional):

::

    [general]
    focus_mode          = "click-to-focus"  # click-to-focus | follows-mouse | sloppy
    repeat_delay        = 600               # ms before key repeat kicks in
    repeat_rate         = 25                # repeats per second
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

Generate a starter file with ``shoestring-wm --write-default-config``.

[general] keys
--------------

**focus_mode** (string, default ``click-to-focus``)
    One of ``click-to-focus``, ``follows-mouse``, ``sloppy``.

**repeat_delay** (integer, default ``600``)
    Milliseconds before a held key starts repeating.

**repeat_rate** (integer, default ``25``)
    Key-repeat rate in repeats per second.

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

    ``minimize``
        Hide the focused window.

    ``unminimize``
        Restore the most-recently-minimized window of the active
        workspace.

    ``close``
        Ask the client to close.

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
