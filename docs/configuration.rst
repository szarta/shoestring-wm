Configuration
=============

shoestring-wm reads a single TOML file at
``$XDG_CONFIG_HOME/shoestring-wm/config.toml`` (falling back to
``$HOME/.config/shoestring-wm/config.toml``). Pass ``--config PATH`` to
override.

Bootstrap a starter file:

.. code-block:: console

    $ shoestring-wm --write-default-config
    wrote default config to /home/you/.config/shoestring-wm/config.toml

The file has two sections — ``[general]`` and a ``[[bindings]]`` array
— both optional. Missing sections take built-in defaults.

The ``[general]`` section
-------------------------

.. list-table::
   :header-rows: 1
   :widths: 18 12 12 58

   * - Key
     - Type
     - Default
     - Meaning
   * - ``focus_mode``
     - string
     - ``"click-to-focus"``
     - One of ``click-to-focus``, ``follows-mouse``, ``sloppy``. Click
       is the default; the other two are accepted by the parser today
       but partially implemented (``follows-mouse`` / ``sloppy`` are on
       the roadmap).
   * - ``repeat_delay``
     - integer ms
     - ``600``
     - Milliseconds before a held key starts repeating. Matches the
       X11 ``Repeat delay`` default.
   * - ``repeat_rate``
     - integer
     - ``25``
     - Key-repeat rate in repeats per second once repeat kicks in.
   * - ``output_scale``
     - float
     - ``1.0``
     - Scale factor advertised to clients via ``wl_output.scale``.
       Whole values (``1.0``, ``2.0``, …) are sent as integer scales;
       non-integer values use fractional scaling (clients supporting
       ``wp_fractional_scale_v1`` see the exact value, others round up).
       Match this to your ``Xft.dpi`` equivalent so text size carries
       over from an X session.

Example::

    [general]
    focus_mode = "click-to-focus"
    repeat_delay = 300
    repeat_rate = 40
    output_scale = 1.5

Bindings
--------

Each ``[[bindings]]`` entry specifies modifiers, a key, and an action::

    [[bindings]]
    mods = ["Super"]
    key = "Return"
    action = { type = "spawn", command = "alacritty" }

    [[bindings]]
    mods = ["Super", "Shift"]
    key = "q"
    action = { type = "quit" }

``mods``
    Array of modifier names, case-insensitive. Recognized values:
    ``Super``, ``Ctrl``, ``Alt``, ``Shift``. Order does not matter; an
    empty array is allowed for un-modified keys.

``key``
    An xkb keysym name as a string. Letters, digits and named keys all
    work: ``"q"``, ``"1"``, ``"Return"``, ``"F5"``, ``"Escape"``,
    ``"space"``. Names follow the standard ``xkbcommon`` table — running
    ``xev`` (or ``wev`` under Wayland) shows the keysym name for any key.

    **Important:** binding lookups use the *pre-modifier* keysym (the
    letter on the key cap), not the shifted form. Bind ``Shift+d`` as
    ``key = "d"`` plus ``mods = ["Shift"]``, not ``key = "D"``.

``action``
    Tagged enum: ``{ type = "...", ... }``. The available actions are
    listed below.

Actions
-------

Each action type and its fields:

``spawn``
    Run a command. Fields:

    - ``command`` — the executable name (looked up on ``$PATH``).
    - ``args`` — optional array of string arguments.

    Example::

        action = { type = "spawn", command = "firefox", args = ["--new-window"] }

``quit``
    Exit the WM cleanly.

``reload-config``
    Re-read the config file from disk. (Config hot-reload via file watch
    is on the roadmap; this action triggers a manual reload today.)

``tile-left``
    Snap the focused window to the left half of its monitor's usable
    rectangle. Re-pressing while already left-tiled restores the saved
    floating geometry — the Openbox "toggle" feel.

``tile-right``
    As ``tile-left`` but the right half.

``maximize``
    Maximize the focused window to the monitor's usable rect (i.e. minus
    layer-shell exclusive zones). Toggle: re-pressing restores the saved
    floating geometry.

``minimize``
    Hide the focused window. The window is *not* destroyed; restore it
    with ``unminimize``.

``unminimize``
    Restore the most-recently-minimized window of the active workspace.

``close``
    Ask the focused window's client to close gracefully (the equivalent
    of clicking the window-manager close button).

``focus-workspace``
    Switch every output to show workspace ``index`` (1-based, 1..=16).
    Example::

        action = { type = "focus-workspace", index = 3 }

``focus-workspace-relative``
    Switch by a relative offset. ``delta = -1`` is previous,
    ``delta = 1`` is next. Saturates at 1 and 16 — no wrapping.

``move-window-to-workspace``
    Move the focused window to ``index`` (1-based) without switching to
    that workspace.

``move-window-to-workspace-relative``
    Move the focused window by ``delta`` (``-1`` / ``1``). Saturates.

``change-vt``
    Switch to Linux virtual terminal ``vt`` (1..=12). Only effective on
    the TTY backend; logged as a no-op under winit. Default binds map
    ``Ctrl+Alt+F1..F12`` to ``change-vt`` for the matching VT.

Pointer bindings
----------------

A few bindings are wired directly in the input layer and are not
user-rebindable today:

================  ===============================================
``Super+Left``    Drag-move the window under the cursor.
``Super+Right``   Drag-resize the window under the cursor (the
                  closest edge follows the pointer).
================  ===============================================

The default keymap
------------------

A reference list of every binding ``--write-default-config`` ships
lives in :doc:`bindings`.
