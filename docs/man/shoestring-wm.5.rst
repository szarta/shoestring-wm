shoestring-wm (config)
======================

Synopsis
--------

``$XDG_CONFIG_HOME/shoestring-wm/config.toml``

Description
-----------

Configuration file for **shoestring-wm**\(1). TOML syntax, two
sections (both optional):

::

    [general]
    focus_mode    = "click-to-focus"     # click-to-focus | follows-mouse | sloppy
    repeat_delay  = 600                  # ms before key repeat kicks in
    repeat_rate   = 25                   # repeats per second
    output_scale  = 1.0                  # wl_output.scale (whole or fractional)

    [[bindings]]
    mods   = ["Super"]
    key    = "Return"
    action = { type = "spawn", command = "alacritty" }

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
        Exit the WM.

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
